//! JavaScript/TypeScript class field declarations.
//!
//! A class field is either data (`count = 0`, `cache: Map<K, V>`) or a
//! function-valued member (`onClick = (e) => {..}`, or a handler wrapped in a
//! call such as `onScroll = throttle((e) => {..}, 100)`). Data fields become
//! [`SymbolKind::Field`] symbols carrying their declared type; function-valued
//! fields become [`SymbolKind::Method`] symbols that own the calls in their
//! body, matching the v1 extractor's class-member split.

use cartograph_domain::{
    ReferenceKind, SymbolId, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingSymbol, def_use, emit_javascript_callable_parameters,
    javascript_decorators, javascript_types, module_system, schema,
    syntax::{has_child_kind, named_children, unquote, visibility},
};

/// Longest field type annotation retained as a type-only signature.
const MAX_FIELD_SIGNATURE_BYTES: usize = 512;

/// The function that implements a function-valued class field.
#[derive(Clone, Copy)]
struct FieldFunction<'tree> {
    callable: Node<'tree>,
    /// The function is an argument of a wrapper call (`throttle(() => ..)`)
    /// rather than the field value itself.
    wrapped: bool,
}

/// One class field resolved to its symbol shape before emission.
#[derive(Clone, Copy)]
struct ClassField<'tree> {
    node: Node<'tree>,
    value: Option<Node<'tree>>,
    function: Option<FieldFunction<'tree>>,
    depth: usize,
}

/// Visit a `public_field_definition` (TypeScript) or `field_definition`
/// (JavaScript) class member.
///
/// Only members of a declared class become symbols, so a field is always
/// contained by its class. Members of an anonymous class expression, and
/// members whose name is computed, numeric, or not a safe literal, have no
/// stable member identity; they keep the generic traversal so their facts
/// stay attributed to the enclosing symbol.
pub(super) fn visit_class_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let Some(name) = field_is_admitted(node, source)
        .then(|| field_name(builder, node))
        .transpose()?
        .flatten()
    else {
        return builder.visit_named_children(node, depth);
    };
    let value = node.child_by_field_name("value");
    let field = ClassField {
        node,
        value,
        function: value.and_then(field_function),
        depth,
    };
    let pending = pending_field_symbol(builder, field, name.clone())?;
    let id = builder.emit_symbol(pending)?;
    capture_field_types(builder, field, &id)?;
    javascript_decorators::capture_declaration_decorators(builder, node, &id)?;
    visit_field_decorators(builder, field)?;
    builder.owners.push(id.clone());
    builder.qualifiers.push(name);
    let visited = visit_field_value(builder, field, &id);
    builder.qualifiers.pop();
    builder.owners.pop();
    visited
}

/// Whether the walker emits `Parameter` symbols for a function value: one
/// bound to an identifier declarator, a direct field method of a declared
/// class, or the first function argument of such a class's wrapper field
/// (`onScroll = throttle((e) => ..)`).
pub(super) fn function_has_parameter_symbols(function: Node<'_>, source: &str) -> bool {
    let Some(parent) = function.parent().filter(|_| is_field_function(function)) else {
        return false;
    };
    match parent.kind() {
        "variable_declarator" => {
            is_bound_callable(function)
                && is_field_value(parent, function)
                && parent
                    .child_by_field_name("name")
                    .is_some_and(|name| matches!(name.kind(), "identifier" | "property_identifier"))
        }
        "public_field_definition" | "field_definition" => {
            field_is_admitted(parent, source) && is_field_value(parent, function)
        }
        "arguments" => is_wrapped_field_function(parent, function, source),
        _ => false,
    }
}

/// Whether `function` is the first function argument of an admitted field's
/// wrapper-call value.
fn is_wrapped_field_function(arguments: Node<'_>, function: Node<'_>, source: &str) -> bool {
    let Some(call) = arguments
        .parent()
        .filter(|call| call.kind() == "call_expression")
    else {
        return false;
    };
    call.parent()
        .filter(|field| matches!(field.kind(), "public_field_definition" | "field_definition"))
        .is_some_and(|field| field_is_admitted(field, source) && is_field_value(field, call))
        && named_children(arguments)
            .find(|argument| is_field_function(*argument))
            .is_some_and(|first| first.id() == function.id())
}

/// Whether the walker emits a member symbol for this field: it belongs to a
/// declared class and has a static, safe member name. This is the single
/// admission rule for field symbols and their parameters.
fn field_is_admitted(field: Node<'_>, source: &str) -> bool {
    in_declared_class(field)
        && field
            .child_by_field_name("name")
            .or_else(|| field.child_by_field_name("property"))
            .is_some_and(|name| match name.kind() {
                "property_identifier" | "private_property_identifier" => true,
                "string" => source
                    .get(name.start_byte()..name.end_byte())
                    .is_some_and(|raw| schema::safe_literal_name(unquote(raw))),
                _ => false,
            })
}

/// Whether `value` is the `value` field of `holder`.
fn is_field_value(holder: Node<'_>, value: Node<'_>) -> bool {
    holder
        .child_by_field_name("value")
        .is_some_and(|child| child.id() == value.id())
}

/// Whether the member sits directly in the body of a class that the walker
/// declares as a [`SymbolKind::Class`].
fn in_declared_class(node: Node<'_>) -> bool {
    node.parent()
        .filter(|body| body.kind() == "class_body")
        .and_then(|body| body.parent())
        .is_some_and(|class| {
            matches!(
                class.kind(),
                "class_declaration" | "abstract_class_declaration"
            )
        })
}

/// The member name of a field with a stable static identity.
fn field_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(name) = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("property"))
    else {
        return Ok(None);
    };
    match name.kind() {
        "property_identifier" | "private_property_identifier" => {
            builder.context.owned_text(name).map(Some)
        }
        // A quoted key is a source literal: it becomes a searchable member
        // name only under the shared literal-name policy.
        "string" => {
            let unquoted = builder.context.owned_unquoted_text(name)?;
            Ok(schema::safe_literal_name(&unquoted).then_some(unquoted))
        }
        _ => Ok(None),
    }
}

/// The function a class field evaluates to, directly or as the first
/// function-valued argument of a wrapper call (v1 `resolveClassFieldFunctionBody`).
fn field_function(value: Node<'_>) -> Option<FieldFunction<'_>> {
    if is_field_function(value) {
        return Some(FieldFunction {
            callable: value,
            wrapped: false,
        });
    }
    if value.kind() != "call_expression" {
        return None;
    }
    let arguments = value.child_by_field_name("arguments")?;
    named_children(arguments)
        .find(|argument| is_field_function(*argument))
        .map(|callable| FieldFunction {
            callable,
            wrapped: true,
        })
}

/// Function values the walker binds to an identifier declarator as a
/// callable with parameter symbols.
fn is_bound_callable(node: Node<'_>) -> bool {
    matches!(node.kind(), "arrow_function" | "function_expression")
}

/// Function values a class field evaluates to as a method: the v1 arrow and
/// function expressions, and generator functions.
fn is_field_function(node: Node<'_>) -> bool {
    is_bound_callable(node) || node.kind() == "generator_function"
}

/// The symbol shape of a field: a method for function values, else a field.
fn pending_field_symbol<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: ClassField<'tree>,
    name: String,
) -> Result<PendingSymbol<'tree>, ExtractError> {
    let private_name = field
        .node
        .child_by_field_name("name")
        .or_else(|| field.node.child_by_field_name("property"))
        .is_some_and(|name| name.kind() == "private_property_identifier");
    let declared_visibility = visibility(field.node, builder.context.source())
        .or_else(|| private_name.then_some(Visibility::Private));
    let static_member = has_child_kind(field.node, "static");
    let pending = match field.function {
        Some(function) => PendingSymbol {
            kind: SymbolKind::Method,
            name,
            span_node: field.node,
            structural_node: function.callable,
            doc_anchor: field.node,
            body_node: function.callable.child_by_field_name("body"),
            declaration_only: false,
            signature: builder.context.callable_signature(function.callable)?,
            export: SymbolExportFlags::new(false, false),
            async_symbol: has_child_kind(function.callable, "async"),
            static_member,
            visibility: declared_visibility,
        },
        None => PendingSymbol {
            kind: SymbolKind::Field,
            name,
            span_node: field.node,
            structural_node: field.node,
            doc_anchor: field.node,
            body_node: None,
            declaration_only: false,
            signature: field_type_signature(builder, field.node)?,
            export: SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member,
            visibility: declared_visibility,
        },
    };
    Ok(pending)
}

/// The declared field type without its `:` marker, when it is literal-free.
fn field_type_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(annotation) = node.child_by_field_name("type") else {
        return Ok(None);
    };
    let raw = builder.context.text(annotation).trim();
    let declared = raw.strip_prefix(':').unwrap_or(raw).trim();
    if declared.is_empty()
        || declared.len() > MAX_FIELD_SIGNATURE_BYTES
        || !callable_signature_is_literal_free(declared)
    {
        return Ok(None);
    }
    builder.context.copy_text(declared).map(Some)
}

/// Type references of the field annotation and, for a direct function
/// value, of its parameters and return type.
fn capture_field_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: ClassField<'_>,
    id: &SymbolId,
) -> Result<(), ExtractError> {
    // A field's types are admitted like a body consumer's: a class type
    // parameter (`value: T`) names no declaration, and a qualified type is
    // recorded whole.
    let types = |root, kind| javascript_types::ConsumerTypes {
        root,
        owner: id,
        kind,
    };
    if let Some(annotation) = field.node.child_by_field_name("type") {
        javascript_types::capture_consumer_types(
            builder,
            types(annotation, ReferenceKind::TypeOf),
        )?;
        module_system::capture_type_position_imports(builder, annotation, id)?;
    }
    let Some(function) = field.function.filter(|function| !function.wrapped) else {
        return Ok(());
    };
    if let Some(parameters) = function.callable.child_by_field_name("parameters") {
        javascript_types::capture_consumer_types(
            builder,
            types(parameters, ReferenceKind::TypeOf),
        )?;
        module_system::capture_type_position_imports(builder, parameters, id)?;
    }
    if let Some(return_type) = function.callable.child_by_field_name("return_type") {
        module_system::capture_type_position_imports(builder, return_type, id)?;
        for kind in [ReferenceKind::Returns, ReferenceKind::TypeOf] {
            javascript_types::capture_consumer_types(builder, types(return_type, kind))?;
        }
    }
    Ok(())
}

/// Decorator expressions run when the class is defined, so their calls stay
/// attributed to the class exactly as they were before fields had symbols.
fn visit_field_decorators(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: ClassField<'_>,
) -> Result<(), ExtractError> {
    for decorator in named_children(field.node).filter(|child| child.kind() == "decorator") {
        builder.visit(decorator, field.depth.saturating_add(1))?;
    }
    Ok(())
}

/// Walk the initializer with the field symbol as owner; a method also
/// records its parameters and def-use sites. A direct function's parameter
/// default values are walked too, so their calls (`(x = build()) => ..`) stay
/// recorded, now owned by the method.
fn visit_field_value(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: ClassField<'_>,
    id: &SymbolId,
) -> Result<(), ExtractError> {
    let depth = field.depth.saturating_add(1);
    let Some(function) = field.function else {
        return match field.value {
            Some(value) => builder.visit(value, depth),
            None => Ok(()),
        };
    };
    emit_javascript_callable_parameters(builder, function.callable)?;
    let body = function.callable.child_by_field_name("body");
    if function.wrapped {
        if let Some(value) = field.value {
            builder.visit(value, depth)?;
        }
    } else {
        visit_parameter_defaults(builder, function.callable, depth)?;
        if let Some(body) = body {
            builder.visit(body, depth)?;
        }
    }
    match body {
        Some(body) => def_use::capture(builder, def_use::DefUseScope::new(body, id)),
        None => Ok(()),
    }
}

/// One step of the parameter-default walk.
enum DefaultStep<'tree> {
    /// A binding position whose nested defaults are still to be found.
    Descend(Node<'tree>),
    /// A runtime expression evaluated when the parameters bind.
    Visit(Node<'tree>),
}

/// Walk, in source order, the expressions a parameter list evaluates when it
/// binds: default values (`x = build()`, `{ y = make() } = {}`) and computed
/// destructuring keys. Annotations are left to the callable type capture, so
/// nothing in them, such as an inline `import()` type, is recorded twice.
fn visit_parameter_defaults(
    builder: &mut ExtractionBuilder<'_, '_>,
    callable: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    // A bare arrow parameter (`x => ..`) has no default.
    let Some(parameters) = callable.child_by_field_name("parameters") else {
        return Ok(());
    };
    let mut pending = vec![DefaultStep::Descend(parameters)];
    while let Some(step) = pending.pop() {
        builder.context.ensure_active()?;
        match step {
            DefaultStep::Visit(expression) => builder.visit(expression, depth)?,
            DefaultStep::Descend(node) => {
                let steps = parameter_default_steps(node);
                pending
                    .try_reserve(steps.len())
                    .map_err(|_| ExtractError::OutputLimit)?;
                pending.extend(steps.into_iter().rev());
            }
        }
    }
    Ok(())
}

/// The nested binding positions and runtime expressions of one parameter
/// list node, in source order.
fn parameter_default_steps(node: Node<'_>) -> Vec<DefaultStep<'_>> {
    let descend = |field| node.child_by_field_name(field).map(DefaultStep::Descend);
    let visit = |field| node.child_by_field_name(field).map(DefaultStep::Visit);
    match node.kind() {
        "required_parameter" | "optional_parameter" => [descend("pattern"), visit("value")]
            .into_iter()
            .flatten()
            .collect(),
        "assignment_pattern" | "object_assignment_pattern" => [descend("left"), visit("right")]
            .into_iter()
            .flatten()
            .collect(),
        "pair_pattern" => {
            let computed_key = node
                .child_by_field_name("key")
                .filter(|key| key.kind() == "computed_property_name")
                .map(DefaultStep::Visit);
            [computed_key, descend("value")]
                .into_iter()
                .flatten()
                .collect()
        }
        "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
            named_children(node).map(DefaultStep::Descend).collect()
        }
        _ => Vec::new(),
    }
}
