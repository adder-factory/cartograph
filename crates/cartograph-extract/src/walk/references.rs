use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId};
use tree_sitter::Node;

use crate::{
    DYNAMIC_DISPATCH_RESOLUTION_PREFIX, ExtractError, ExtractedReference,
    RUST_SELF_RECEIVER_RESOLUTION_PREFIX, TYPE_QUERY_VALUE_RESOLUTION_PREFIX,
};

use super::{
    ExtractionBuilder, PendingReference, module_system,
    syntax::{
        descendants_including_root, is_call_or_construction_target, is_rust_turbofish_callee,
        named_children, reference_type_node, span_for, starts_uppercase,
    },
};

/// Longest callee text kept as a durable reference name; a wider Go callee is
/// named by its member alone.
pub(super) const MAX_DURABLE_REFERENCE_NAME_BYTES: usize = 4_096;
/// Deepest static member chain (`a.b.c`) followed.
const MAX_STATIC_CHAIN_DEPTH: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InvocationKind {
    Call,
    Construction,
}

struct NodeReference<'tree> {
    owner: Option<SymbolId>,
    name: Node<'tree>,
    kind: ReferenceKind,
    span: Node<'tree>,
}

#[derive(Clone, Copy)]
struct TypeTreeCapture<'tree, 'owner> {
    root: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
}

pub(super) fn capture_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for child in named_children(node) {
        builder.context.ensure_active()?;
        match child.kind() {
            "extends_type_clause" => capture_named_heritage_targets(
                builder,
                TypeTreeCapture {
                    root: child,
                    owner,
                    kind: ReferenceKind::Extends,
                },
            )?,
            "class_heritage" => capture_class_heritage(builder, child, owner)?,
            _ => {}
        }
    }
    Ok(())
}

fn capture_class_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    heritage: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for clause in named_children(heritage) {
        builder.context.ensure_active()?;
        match clause.kind() {
            "extends_clause" => {
                if let Some(target) = clause
                    .child_by_field_name("value")
                    .filter(|target| !is_receiver_chain(*target))
                    && heritage_base_resolves(builder, target)?
                {
                    push_node_reference(
                        builder,
                        NodeReference {
                            owner: Some(owner.clone()),
                            name: target,
                            kind: ReferenceKind::Extends,
                            span: target,
                        },
                    )?;
                }
            }
            "implements_clause" => capture_named_heritage_targets(
                builder,
                TypeTreeCapture {
                    root: clause,
                    owner,
                    kind: ReferenceKind::Implements,
                },
            )?,
            // Plain JavaScript: `class A extends B` puts the base expression
            // directly under `class_heritage`. Only a static name is a base;
            // a computed mixin (`extends mixin(B)`) has no declaration target.
            "identifier" | "member_expression"
                if !is_receiver_chain(clause) && heritage_base_resolves(builder, clause)? =>
            {
                if let Some(name) = static_member_chain_name(builder, clause)? {
                    push_reference(
                        builder,
                        PendingReference {
                            owner: Some(owner.clone()),
                            name,
                            kind: ReferenceKind::Extends,
                            node: clause,
                        },
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Emit the named targets of an extends or implements clause in child order.
fn capture_named_heritage_targets(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeTreeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    for target in named_children(input.root) {
        builder.context.ensure_active()?;
        if let Some(name_node) = reference_type_node(target) {
            push_node_reference(
                builder,
                NodeReference {
                    owner: Some(input.owner.clone()),
                    name: name_node,
                    kind: input.kind,
                    span: name_node,
                },
            )?;
        }
    }
    Ok(())
}

pub(super) fn capture_callable_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    if let Some(parameters) = node
        .child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("parameter"))
    {
        if builder.context.snapshot.language() == SourceLanguage::Python {
            capture_python_parameter_types(builder, parameters, owner)?;
        } else {
            capture_type_nodes(builder, parameters, owner)?;
            module_system::capture_type_position_imports(builder, parameters, owner)?;
        }
    }
    if let Some(return_type) = node
        .child_by_field_name("return_type")
        .or_else(|| node.child_by_field_name("result"))
    {
        module_system::capture_type_position_imports(builder, return_type, owner)?;
        capture_type_tree(
            builder,
            TypeTreeCapture {
                root: return_type,
                owner,
                kind: ReferenceKind::Returns,
            },
        )?;
        capture_type_tree(
            builder,
            TypeTreeCapture {
                root: return_type,
                owner,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    Ok(())
}

fn capture_python_parameter_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for parameter in descendants_including_root(parameters) {
        builder.context.ensure_active()?;
        let Some(type_node) = parameter.child_by_field_name("type") else {
            continue;
        };
        capture_type_tree(
            builder,
            TypeTreeCapture {
                root: type_node,
                owner,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    Ok(())
}

fn capture_type_tree(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeTreeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let language = builder.context.snapshot.language();
    for target in descendants_including_root(input.root) {
        builder.context.ensure_active()?;
        let Some(name) = import_type_member(language, target)
            .or_else(|| is_type_name(language, target).then_some(target))
        else {
            continue;
        };
        push_node_reference(
            builder,
            NodeReference {
                owner: Some(input.owner.clone()),
                name,
                kind: input.kind,
                span: name,
            },
        )?;
    }
    Ok(())
}

fn is_type_name(language: SourceLanguage, node: Node<'_>) -> bool {
    node.kind() == "type_identifier"
        || (language == SourceLanguage::Python && node.kind() == "identifier")
}

pub(super) fn capture_type_nodes(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for target in descendants_including_root(node) {
        builder.context.ensure_active()?;
        capture_type_node(
            builder,
            TypeNodeCapture {
                target,
                owner,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    Ok(())
}

/// One node of a type subtree and the reference it would record.
#[derive(Clone, Copy)]
pub(super) struct TypeNodeCapture<'tree, 'owner> {
    pub(super) target: Node<'tree>,
    pub(super) owner: &'owner SymbolId,
    /// `type_of`, or `returns` for a return annotation.
    pub(super) kind: ReferenceKind,
}

/// Record the reference one node of a type subtree makes, if any: a type
/// name, an inline import type's member, or (for `type_of`) a
/// `typeof value` query.
pub(super) fn capture_type_node(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: TypeNodeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let TypeNodeCapture {
        target,
        owner,
        kind,
    } = capture;
    let language = builder.context.snapshot.language();
    if let Some(property) = import_type_member(language, target) {
        return push_node_reference(
            builder,
            NodeReference {
                owner: Some(owner.clone()),
                name: property,
                kind,
                span: property,
            },
        );
    }
    if target.kind() == "type_identifier" {
        return push_node_reference(
            builder,
            NodeReference {
                owner: Some(owner.clone()),
                name: target,
                kind,
                span: target,
            },
        );
    }
    let type_query_value = kind == ReferenceKind::TypeOf
        && matches!(language, SourceLanguage::TypeScript | SourceLanguage::Tsx)
        && target.kind() == "identifier"
        && target
            .parent()
            .is_some_and(|parent| parent.kind() == "type_query")
        && !zod_infer_type_query(builder, target);
    if !type_query_value {
        return Ok(());
    }
    let name = builder.context.owned_text(target)?;
    let capacity = TYPE_QUERY_VALUE_RESOLUTION_PREFIX
        .len()
        .checked_add(name.len())
        .ok_or(ExtractError::OutputLimit)?;
    let mut resolution_name = String::new();
    resolution_name
        .try_reserve(capacity)
        .map_err(|_| ExtractError::OutputLimit)?;
    resolution_name.push_str(TYPE_QUERY_VALUE_RESOLUTION_PREFIX);
    resolution_name.push_str(&name);
    builder.emit_reference(ExtractedReference {
        owner: Some(owner.clone()),
        name,
        resolution_name: Some(resolution_name),
        kind: ReferenceKind::TypeOf,
        span: span_for(target)?,
    })
}

/// The named type of a TypeScript inline import type, such as `Options` in
/// `opts?: import('./opts').Options`, when `node` is that member in a type
/// position. A runtime `import('./x').then(..)` is never a type.
fn import_type_member(language: SourceLanguage, node: Node<'_>) -> Option<Node<'_>> {
    if !matches!(language, SourceLanguage::TypeScript | SourceLanguage::Tsx)
        || node.kind() != "member_expression"
        || !node
            .parent()
            .is_some_and(|parent| holds_type_at(parent, node))
    {
        return None;
    }
    let imports_module = node
        .child_by_field_name("object")
        .filter(|object| object.kind() == "call_expression")
        .and_then(|call| call.child_by_field_name("function"))
        .is_some_and(|function| function.kind() == "import");
    node.child_by_field_name("property")
        .filter(|property| imports_module && property.kind() == "property_identifier")
}

/// Whether `child` of `parent` sits in a TypeScript type position: under a
/// type-holding node, or as the asserted type (never the operand) of an
/// `as`/`satisfies` expression (`{} as import('./m').T`).
pub(super) fn holds_type_at(parent: Node<'_>, child: Node<'_>) -> bool {
    match parent.kind() {
        "as_expression" | "satisfies_expression" => {
            asserted_type(parent).is_some_and(|asserted| asserted.id() == child.id())
        }
        kind => is_type_position(kind),
    }
}

/// The type an `as`/`satisfies` expression asserts: its named child after
/// the operand. `x as const` names no type.
pub(super) fn asserted_type(assertion: Node<'_>) -> Option<Node<'_>> {
    let mut children = named_children(assertion).filter(|child| child.kind() != "comment");
    children.next()?;
    children.last()
}

/// Whether a node of this kind holds its children in a TypeScript type
/// position (an annotation, type argument, type operator, or alias value).
fn is_type_position(kind: &str) -> bool {
    kind.ends_with("_type")
        || matches!(
            kind,
            "type_annotation"
                | "type_arguments"
                | "type_alias_declaration"
                | "type_query"
                | "constraint"
                | "default_type"
                | "asserts_annotation"
                | "type_predicate_annotation"
        )
}

fn zod_infer_type_query(builder: &ExtractionBuilder<'_, '_>, target: Node<'_>) -> bool {
    let mut current = target.parent();
    for _ in 0..8 {
        let Some(node) = current else {
            return false;
        };
        if matches!(node.kind(), "generic_type" | "type_arguments")
            && builder.context.text(node).contains("z.infer")
        {
            return true;
        }
        if matches!(node.kind(), "type_alias_declaration" | "type_alias") {
            return false;
        }
        current = node.parent();
    }
    false
}

pub(super) fn capture_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    invocation: InvocationKind,
) -> Result<(), ExtractError> {
    let Some(capture) =
        InvocationCapture::new(node, invocation, builder.context.snapshot.language())
    else {
        return Ok(());
    };
    if module_system::is_static_commonjs_binding_call(builder, node) {
        return Ok(());
    }
    if capture_rust_receiver_invocation(builder, capture)? {
        return Ok(());
    }
    if capture.shape.invocation == InvocationKind::Call
        && anonymous_call_target(capture.shape.language, capture.target, 0)
    {
        return Ok(());
    }
    if capture_overwide_go_invocation(builder, capture)? {
        return Ok(());
    }
    if capture_javascript_dispatch(builder, capture)? {
        return Ok(());
    }
    capture_default_invocation(builder, capture)
}

fn capture_rust_receiver_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: InvocationCapture<'_>,
) -> Result<bool, ExtractError> {
    if capture.shape.invocation != InvocationKind::Call
        || capture.shape.language != SourceLanguage::Rust
    {
        return Ok(false);
    }
    let Some(resolution_name) = rust_receiver_call_resolution(builder, capture.target)? else {
        return Ok(false);
    };
    let owner = builder.owners.last().cloned();
    let name = builder.context.owned_text(capture.target)?;
    builder.emit_reference(ExtractedReference {
        owner,
        name,
        resolution_name: Some(resolution_name),
        kind: capture.reference_kind,
        span: span_for(capture.target)?,
    })?;
    Ok(true)
}

fn capture_overwide_go_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: InvocationCapture<'_>,
) -> Result<bool, ExtractError> {
    if capture.shape.invocation != InvocationKind::Call
        || capture.shape.language != SourceLanguage::Go
        || capture
            .target
            .end_byte()
            .saturating_sub(capture.target.start_byte())
            <= MAX_DURABLE_REFERENCE_NAME_BYTES
    {
        return Ok(false);
    }
    if capture.target.kind() == "selector_expression"
        && let Some(field) = capture.target.child_by_field_name("field")
    {
        let name = builder.context.owned_text(field)?;
        let resolution_name = dynamic_dispatch_resolution(builder, &name)?;
        builder.emit_reference(ExtractedReference {
            owner: builder.owners.last().cloned(),
            name,
            resolution_name: Some(resolution_name),
            kind: capture.reference_kind,
            span: span_for(field)?,
        })?;
    }
    Ok(true)
}

fn capture_javascript_dispatch(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: InvocationCapture<'_>,
) -> Result<bool, ExtractError> {
    if capture.shape.invocation != InvocationKind::Call
        || !javascript_call_syntax(capture.shape.language)
    {
        return Ok(false);
    }
    let Some(dispatch) = javascript_dynamic_member_call_resolution(builder, capture.target)? else {
        return Ok(false);
    };
    let owner = builder.owners.last().cloned();
    builder.emit_reference(ExtractedReference {
        owner,
        name: dispatch.name,
        resolution_name: Some(dispatch.resolution_name),
        kind: capture.reference_kind,
        span: span_for(dispatch.span)?,
    })?;
    // A statically named chain can still resolve exactly through an import
    // binding, while its terminal method also represents interface/runtime
    // dispatch. Retain both bounded facts; dynamic resolution only chooses a
    // unique externally visible target and otherwise abstains.
    Ok(!static_javascript_member_chain(capture.target, 0))
}

fn capture_default_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: InvocationCapture<'_>,
) -> Result<(), ExtractError> {
    let Some((name_node, reference_node)) =
        invocation_reference_nodes(capture.shape, capture.target, capture.expression)
    else {
        return Ok(());
    };
    let owner = builder.owners.last().cloned();
    push_node_reference(
        builder,
        NodeReference {
            owner,
            name: name_node,
            kind: capture.reference_kind,
            span: reference_node,
        },
    )
}

#[derive(Clone, Copy)]
struct InvocationCapture<'tree> {
    expression: Node<'tree>,
    target: Node<'tree>,
    shape: InvocationShape,
    reference_kind: ReferenceKind,
}

impl<'tree> InvocationCapture<'tree> {
    fn new(
        expression: Node<'tree>,
        invocation: InvocationKind,
        language: SourceLanguage,
    ) -> Option<Self> {
        let (target_field, reference_kind) = match invocation {
            InvocationKind::Call => ("function", ReferenceKind::Calls),
            InvocationKind::Construction => ("constructor", ReferenceKind::Instantiates),
        };
        let target = rust_turbofish_callee(language, expression.child_by_field_name(target_field)?);
        Some(Self {
            expression,
            target,
            shape: InvocationShape {
                language,
                invocation,
            },
            reference_kind,
        })
    }
}

/// The callee of a Rust turbofish call: `f::<T>(..)`, `a::f::<T>(..)`, and
/// `x.f::<T>(..)` call `f`, whose type arguments name no callee, so the
/// reference names and resolves the function the way an unparameterized
/// call does.
fn rust_turbofish_callee(language: SourceLanguage, target: Node<'_>) -> Node<'_> {
    if language == SourceLanguage::Rust && target.kind() == "generic_function" {
        target.child_by_field_name("function").unwrap_or(target)
    } else {
        target
    }
}

/// Language and call form one invocation reference is resolved under.
#[derive(Clone, Copy)]
struct InvocationShape {
    language: SourceLanguage,
    invocation: InvocationKind,
}

fn invocation_reference_nodes<'tree>(
    shape: InvocationShape,
    target: Node<'tree>,
    expression: Node<'tree>,
) -> Option<(Node<'tree>, Node<'tree>)> {
    let InvocationShape {
        language,
        invocation,
    } = shape;
    match (invocation, javascript_call_syntax(language)) {
        (InvocationKind::Call, true) => {
            normalized_javascript_call_target(target, 0).map(|name| (name, name))
        }
        (InvocationKind::Call, false) => Some((target, target)),
        (InvocationKind::Construction, true) => {
            normalized_javascript_construction_target(target, 0).map(|name| (name, expression))
        }
        (InvocationKind::Construction, false) => Some((target, expression)),
    }
}

/// Languages whose calls use JavaScript call and member-expression syntax,
/// including `ArkTS`, whose grammar keeps TypeScript's expression node kinds.
const fn javascript_call_syntax(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::ArkTs
    )
}

fn anonymous_call_target(language: SourceLanguage, target: Node<'_>, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match (language, target.kind()) {
        (SourceLanguage::Rust, "closure_expression")
        | (SourceLanguage::Go, "func_literal")
        | (SourceLanguage::Python, "lambda") => true,
        (_, "parenthesized_expression") => {
            let mut children = named_children(target);
            let Some(child) = children.next() else {
                return false;
            };
            children.next().is_none()
                && anonymous_call_target(language, child, depth.saturating_add(1))
        }
        _ => false,
    }
}

fn rust_receiver_call_resolution(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(target) = rust_receiver_target(target, 0) else {
        return Ok(None);
    };
    let Some(field) = target.child_by_field_name("field") else {
        return Ok(None);
    };
    let snapshot = builder.context.snapshot;
    let field = snapshot
        .source()
        .get(field.start_byte()..field.end_byte())
        .unwrap_or_default()
        .trim();
    if target
        .child_by_field_name("value")
        .is_some_and(|receiver| receiver.kind() == "self")
    {
        return rust_self_receiver_resolution(builder, target, field);
    }
    if field.is_empty() {
        return Ok(None);
    }
    dynamic_dispatch_resolution(builder, field).map(Some)
}

/// Resolution hint for a `self.field(..)` call whose receiver sits at `anchor`,
/// naming the enclosing impl's nominal type when syntax proves it.
pub(super) fn rust_self_receiver_resolution(
    builder: &mut ExtractionBuilder<'_, '_>,
    anchor: Node<'_>,
    field: &str,
) -> Result<Option<String>, ExtractError> {
    let nominal = rust_self_nominal_type(builder, anchor)?;
    let type_name = nominal.as_deref();
    if field.is_empty() {
        return Ok(None);
    }
    let capacity = RUST_SELF_RECEIVER_RESOLUTION_PREFIX
        .len()
        .checked_add(type_name.map_or(0, |name| name.len().saturating_add(2)))
        .and_then(|bytes| bytes.checked_add(field.len()))
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(capacity)?;
    let mut resolution = String::new();
    resolution
        .try_reserve_exact(capacity)
        .map_err(|_| ExtractError::OutputLimit)?;
    resolution.push_str(RUST_SELF_RECEIVER_RESOLUTION_PREFIX);
    if let Some(type_name) = type_name {
        resolution.push_str(type_name);
        resolution.push_str("::");
    }
    resolution.push_str(field);
    Ok(Some(resolution))
}

fn rust_self_nominal_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut ancestor = target.parent();
    let mut receiver_seen = false;
    for _ in 0..=builder.maximum_ast_depth {
        builder.context.ensure_active()?;
        let Some(node) = ancestor else {
            break;
        };
        if node.kind() == "function_item" {
            if receiver_seen || !rust_function_has_self_parameter(node) {
                return Ok(None);
            }
            receiver_seen = true;
        }
        if node.kind() == "impl_item" && receiver_seen {
            return rust_impl_nominal_type(builder, node);
        }
        ancestor = node.parent();
    }
    Ok(None)
}

fn rust_function_has_self_parameter(function: Node<'_>) -> bool {
    function
        .child_by_field_name("parameters")
        .is_some_and(|parameters| {
            named_children(parameters).any(|parameter| parameter.kind() == "self_parameter")
        })
}

fn rust_impl_nominal_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    implementation: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(mut nominal) = implementation.child_by_field_name("type") else {
        return Ok(None);
    };
    if nominal.kind() == "generic_type" {
        let Some(base) = nominal.child_by_field_name("type") else {
            return Ok(None);
        };
        nominal = base;
    }
    if !matches!(nominal.kind(), "type_identifier" | "scoped_type_identifier") {
        return Ok(None);
    }
    let receiver_name = builder.context.owned_text(nominal)?;
    if rust_impl_parameter_shadows_type(builder, implementation, &receiver_name)?
        || !receiver_name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | ':' | '#'))
    {
        return Ok(None);
    }
    rust_self_type_in_scope(builder, implementation, nominal)
}

fn rust_impl_parameter_shadows_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    implementation: Node<'_>,
    receiver_name: &str,
) -> Result<bool, ExtractError> {
    let Some(parameters) = implementation.child_by_field_name("type_parameters") else {
        return Ok(false);
    };
    let receiver_root = receiver_name.split("::").next();
    for parameter in named_children(parameters) {
        builder.context.ensure_active()?;
        if parameter
            .child_by_field_name("name")
            .is_some_and(|name| Some(builder.context.text(name)) == receiver_root)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rust_self_type_in_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    implementation: Node<'_>,
    nominal: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(scope) = implementation.parent() else {
        return Ok(None);
    };
    if !matches!(scope.kind(), "source_file" | "declaration_list") {
        // A block-local type needs its enclosing function identity, not a module path.
        return Ok(None);
    }
    let local_type = rust_scope_declares_type(builder, scope, nominal)?;
    let mut nominal_name = builder.context.owned_text(nominal)?;
    if !local_type && nominal.kind() == "type_identifier" {
        let Some(imported) = super::polyglot::rust_nominal_import(builder, scope, &nominal_name)?
        else {
            return Ok(None);
        };
        nominal_name = imported;
    }
    if !local_type && !nominal_name.starts_with("self::") && !nominal_name.starts_with("super::") {
        return Ok(Some(nominal_name));
    }
    let Some(mut modules) = rust_self_inline_modules(builder, scope)? else {
        return Ok(None);
    };
    let mut relative = nominal_name.strip_prefix("self::").unwrap_or(&nominal_name);
    let mut parents = 0;
    while let Some(remainder) = relative.strip_prefix("super::") {
        if modules.pop().is_none() {
            parents += 1;
        }
        relative = remainder;
    }
    let mut name = String::new();
    for component in std::iter::repeat_n("super", parents)
        .chain(std::iter::once("self").take(usize::from(parents == 0)))
        .chain(
            modules
                .into_iter()
                .map(|module| builder.context.text(module)),
        )
        .chain(std::iter::once(relative))
    {
        let length = name
            .len()
            .checked_add(component.len())
            .and_then(|bytes| bytes.checked_add(2))
            .ok_or(ExtractError::OutputLimit)?;
        builder.context.budget.ensure_string_length(length)?;
        name.try_reserve(component.len().saturating_add(2))
            .map_err(|_| ExtractError::OutputLimit)?;
        if !name.is_empty() {
            name.push_str("::");
        }
        name.push_str(component);
    }
    Ok(Some(name))
}

fn rust_scope_declares_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Node<'_>,
    nominal: Node<'_>,
) -> Result<bool, ExtractError> {
    if nominal.kind() != "type_identifier" {
        return Ok(false);
    }
    for declaration in named_children(scope) {
        builder.context.ensure_active()?;
        if matches!(
            declaration.kind(),
            "struct_item" | "enum_item" | "union_item" | "type_item"
        ) && declaration
            .child_by_field_name("name")
            .is_some_and(|name| builder.context.text(name) == builder.context.text(nominal))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rust_self_inline_modules<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Node<'tree>,
) -> Result<Option<Vec<Node<'tree>>>, ExtractError> {
    let mut modules = Vec::new();
    let mut ancestor = Some(scope);
    for _ in 0..=builder.maximum_ast_depth {
        builder.context.ensure_active()?;
        let Some(node) = ancestor else { break };
        if node.kind() == "function_item" {
            return Ok(None);
        }
        if node.kind() == "mod_item"
            && let Some(name) = node.child_by_field_name("name")
        {
            modules
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            modules.push(name);
        }
        ancestor = node.parent();
    }
    modules.reverse();
    Ok(Some(modules))
}

fn rust_receiver_target(target: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > 8 {
        return None;
    }
    match target.kind() {
        "field_expression" => Some(target),
        "generic_function" => target
            .child_by_field_name("function")
            .or_else(|| named_children(target).next())
            .and_then(|function| rust_receiver_target(function, depth.saturating_add(1))),
        _ => None,
    }
}

struct JavascriptDispatchReference<'tree> {
    span: Node<'tree>,
    name: String,
    resolution_name: String,
}

fn javascript_dynamic_member_call_resolution<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'tree>,
) -> Result<Option<JavascriptDispatchReference<'tree>>, ExtractError> {
    let (span, name) = match target.kind() {
        "member_expression" => {
            let Some(property) = target
                .child_by_field_name("property")
                .and_then(|property| normalized_javascript_call_target(property, 0))
            else {
                return Ok(None);
            };
            (property, builder.context.owned_text(property)?)
        }
        "subscript_expression" => {
            let Some(index) = target.child_by_field_name("index") else {
                return Ok(None);
            };
            let Some(name) = static_javascript_dispatch_key(builder.context.text(index)) else {
                return Ok(None);
            };
            builder.context.budget.ensure_string_length(name.len())?;
            (index, name.to_owned())
        }
        _ => return Ok(None),
    };
    if name.is_empty() {
        return Ok(None);
    }
    let resolution_name = dynamic_dispatch_resolution(builder, &name)?;
    Ok(Some(JavascriptDispatchReference {
        span,
        name,
        resolution_name,
    }))
}

fn static_javascript_dispatch_key(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let key = raw
        .strip_circumfix('"', '"')
        .or_else(|| raw.strip_circumfix('\'', '\''))
        .or_else(|| raw.strip_circumfix('`', '`'))?;
    let mut characters = key.chars();
    let first = characters.next()?;
    (key.len() <= MAX_DURABLE_REFERENCE_NAME_BYTES
        && !super::specifier_safety::specifier_may_carry_credential(key)
        && (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && characters.all(|character| {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        }))
    .then_some(key)
}

pub(super) fn dynamic_dispatch_resolution(
    builder: &ExtractionBuilder<'_, '_>,
    name: &str,
) -> Result<String, ExtractError> {
    let capacity = DYNAMIC_DISPATCH_RESOLUTION_PREFIX
        .len()
        .checked_add(name.len())
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(capacity)?;
    let mut resolution_name = String::new();
    resolution_name
        .try_reserve_exact(capacity)
        .map_err(|_| ExtractError::OutputLimit)?;
    resolution_name.push_str(DYNAMIC_DISPATCH_RESOLUTION_PREFIX);
    resolution_name.push_str(name);
    Ok(resolution_name)
}

fn normalized_javascript_call_target(target: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > 64 {
        return None;
    }
    match target.kind() {
        "identifier" | "property_identifier" | "private_property_identifier" | "this" | "super" => {
            Some(target)
        }
        "member_expression" if static_javascript_member_chain(target, 0) => Some(target),
        "member_expression" => target.child_by_field_name("property").and_then(|property| {
            normalized_javascript_call_target(property, depth.saturating_add(1))
        }),
        "parenthesized_expression" => {
            let mut children = named_children(target);
            let child = children.next()?;
            if children.next().is_some() {
                return None;
            }
            normalized_javascript_call_target(child, depth.saturating_add(1))
        }
        // The inner call already records its statically known target. The value it returns is
        // dynamically callable, while arrow/function IIFEs have no stable declaration target.
        _ => None,
    }
}

fn normalized_javascript_construction_target(target: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > 64 {
        return None;
    }
    match target.kind() {
        "identifier" => Some(target),
        "member_expression" if static_javascript_member_chain(target, 0) => Some(target),
        "parenthesized_expression" => {
            let mut children = named_children(target);
            let child = children.next()?;
            if children.next().is_some() {
                return None;
            }
            normalized_javascript_construction_target(child, depth.saturating_add(1))
        }
        // Anonymous classes, calls returning constructors, and computed member
        // expressions have no exact declaration target. Their named type usages are
        // still captured independently by the normal type-reference walk.
        _ => None,
    }
}

fn static_javascript_member_chain(node: Node<'_>, depth: usize) -> bool {
    if depth > MAX_STATIC_CHAIN_DEPTH {
        return false;
    }
    match node.kind() {
        "identifier" | "property_identifier" | "private_property_identifier" | "this" | "super" => {
            true
        }
        "member_expression" => {
            node.child_by_field_name("object").is_some_and(|object| {
                static_javascript_member_chain(object, depth.saturating_add(1))
            }) && node
                .child_by_field_name("property")
                .is_some_and(|property| {
                    static_javascript_member_chain(property, depth.saturating_add(1))
                })
        }
        _ => false,
    }
}

/// Whether a JavaScript-family base expression names what the resolver
/// would bind it to: a base an enclosing parameter or local rebinds
/// (`function mixin(Base) { return class extends Base {} }`) is that binding,
/// not a module class of the same name. Other bases are kept as they are.
fn heritage_base_resolves(
    builder: &mut ExtractionBuilder<'_, '_>,
    base: Node<'_>,
) -> Result<bool, ExtractError> {
    if !module_system::is_javascript_family(builder.context.snapshot.language())
        || !matches!(base.kind(), "identifier" | "member_expression")
    {
        return Ok(true);
    }
    super::javascript_scopes::static_chain_resolves(builder, base)
}

/// Whether a static chain is rooted at `this` or `super`, an instance or
/// parent member rather than a declaration (`class P extends this.base`).
fn is_receiver_chain(node: Node<'_>) -> bool {
    let mut current = node;
    for _ in 0..MAX_STATIC_CHAIN_DEPTH {
        match current.kind() {
            "member_expression" => match current.child_by_field_name("object") {
                Some(object) => current = object,
                None => return false,
            },
            kind => return matches!(kind, "this" | "super"),
        }
    }
    true
}

/// The dotted name of a static identifier/member chain (`ng.Input`), built
/// from its name tokens so comments, whitespace, and optional-chaining
/// punctuation inside the chain never become part of a reference name.
pub(super) fn static_member_chain_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if !static_javascript_member_chain(node, 0) {
        return Ok(None);
    }
    let mut segments = Vec::new();
    let mut current = node;
    while current.kind() == "member_expression" {
        let (Some(object), Some(property)) = (
            current.child_by_field_name("object"),
            current.child_by_field_name("property"),
        ) else {
            return Ok(None);
        };
        segments
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        segments.push(property);
        current = object;
    }
    segments
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    segments.push(current);
    let length = segments.iter().try_fold(segments.len(), |length, segment| {
        length.checked_add(builder.context.text(*segment).trim().len())
    });
    let length = length.ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(length)?;
    let mut name = String::new();
    name.try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    for segment in segments.iter().rev() {
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(builder.context.text(*segment).trim());
    }
    Ok(Some(name))
}

pub(super) fn capture_jsx_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(target) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(target)?;
    if !starts_uppercase(&name) {
        return Ok(());
    }
    push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::References,
            node: target,
        },
    )
}

pub(super) fn capture_field_access(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if is_call_or_construction_target(node) {
        return Ok(());
    }
    capture_member_field(builder, node, "property")
}

pub(super) fn capture_member_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    field_name: &str,
) -> Result<(), ExtractError> {
    if is_call_or_construction_target(node)
        || is_rust_turbofish_callee(node)
        || is_commonjs_require_selection(builder, node)
    {
        return Ok(());
    }
    let Some(property) = node.child_by_field_name(field_name) else {
        return Ok(());
    };
    let owner = builder.owners.last().cloned();
    push_node_reference(
        builder,
        NodeReference {
            owner,
            name: property,
            kind: ReferenceKind::FieldAccess,
            span: property,
        },
    )
}

fn is_commonjs_require_selection(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    if node.kind() != "member_expression"
        || !matches!(
            builder.context.snapshot.language(),
            SourceLanguage::TypeScript
                | SourceLanguage::Tsx
                | SourceLanguage::JavaScript
                | SourceLanguage::Jsx
        )
    {
        return false;
    }
    node.child_by_field_name("object")
        .filter(|object| object.kind() == "call_expression")
        .and_then(|call| call.child_by_field_name("function"))
        .is_some_and(|function| builder.context.text(function).trim() == "require")
}

fn push_node_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    reference: NodeReference<'_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(reference.name)?;
    push_reference(
        builder,
        PendingReference {
            owner: reference.owner,
            name,
            kind: reference.kind,
            node: reference.span,
        },
    )
}

pub(super) fn push_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: PendingReference<'_>,
) -> Result<(), ExtractError> {
    if pending.name.is_empty() {
        return Ok(());
    }
    builder.emit_reference(ExtractedReference {
        owner: pending.owner,
        name: pending.name,
        resolution_name: None,
        kind: pending.kind,
        span: span_for(pending.node)?,
    })
}
