//! Shared fallback: every unmodeled binder shadows, and writes invalidate constructors.

use super::{
    AstVisitBudget, BindingType, ExtractError, ExtractionBuilder, ExtractionContext, NamedBind,
    Node, ScopeKind, SourceLanguage, SyntaxIndex, TypeOrigin, TypeQuery, Visit, identifier,
    named_children, node_text, nominal, push_visit,
};

pub(super) struct FallbackBinding<'tree> {
    visit: Visit<'tree>,
    pattern: Node<'tree>,
    write: bool,
}

pub(super) fn append_non_methods(
    builder: &mut ExtractionBuilder<'_, '_>,
    non_methods: &mut Vec<crate::ExtractedReceiverBinding>,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    builder.context.budget.reserve_working_bytes(
        u64::try_from(non_methods.len())
            .map_err(|_| ExtractError::OutputLimit)?
            .saturating_mul(
                u64::try_from(std::mem::size_of::<crate::ExtractedReceiverBinding>())
                    .map_err(|_| ExtractError::OutputLimit)?,
            ),
    )?;
    builder
        .facts
        .receiver_bindings
        .try_reserve_exact(non_methods.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.facts.receiver_bindings.append(non_methods);
    Ok(())
}

pub(super) fn scope_kind(node: Node<'_>) -> Option<ScopeKind> {
    match node.kind() {
        "arrow_function"
        | "function_expression"
        | "anonymous_method_expression"
        | "constructor_signature"
        | "block_literal"
        | "script_block_expression"
        | "local_function_statement" => Some(ScopeKind::Callable),
        "for_statement" | "for_in_statement" | "foreach" | "switch_section" | "switch_entry"
        | "match_case" | "match_arm" | "case_clause" | "case" | "query_expression"
        | "exceptionHandler" => Some(ScopeKind::Block),
        _ => None,
    }
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    if !nominal::supported(context.snapshot.language()) {
        return Ok(());
    }
    if parameter_container(visit.node.kind()) {
        return parameters(index, context, visit);
    }
    if visit.node.kind() == "function_body" {
        sibling_parameters(index, context, visit)?;
    }
    let pattern = binding_pattern(visit.node);
    if let Some(pattern) = pattern {
        record(index, context, (visit, pattern, false))?;
    }
    if let Some(pattern) = iteration_pattern(visit.node) {
        let write = iteration_writes(visit.node);
        record(index, context, (visit, pattern, write))?;
    }
    if let Some(pattern) = write_pattern(visit.node) {
        record(index, context, (visit, pattern, true))?;
    }
    collect_unmodeled(index, context, visit)?;
    Ok(())
}

fn collect_unmodeled<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let node = visit.node;
    let pattern = if pattern_kind(node.kind()) {
        if node.parent().is_some_and(|node| pattern_kind(node.kind())) {
            return Ok(());
        }
        Some(node)
    } else if unmodeled_binder(node.kind()) {
        node.child_by_field_name("name")
            .or_else(|| node.child_by_field_name("variable"))
            .or_else(|| node.child_by_field_name("left"))
            .or_else(|| binding_child(node))
            .or(Some(node))
    } else {
        node.child_by_field_name("declarator")
            .or_else(|| ambiguous_declaration(node, context.snapshot.language()))
    };
    if let Some(pattern) = pattern {
        record(
            index,
            context,
            (visit, pattern, pattern_writes(node.kind())),
        )?;
    }
    Ok(())
}

fn pattern_kind(kind: &str) -> bool {
    kind.contains("pattern") && kind != "binding_pattern_kind"
}

fn pattern_writes(kind: &str) -> bool {
    matches!(
        kind,
        "pattern_assignment" | "match_pattern" | "test_pattern" | "varAssignDef"
    )
}

fn unmodeled_binder(kind: &str) -> bool {
    kind.contains("parameter") && !parameter_container(kind) && !kind.contains("modifier")
        || matches!(
            kind,
            "multi_variable_declaration"
                | "declaration_expression"
                | "exceptionHandler"
                | "varDef"
                | "varAssignDef"
                | "from_clause"
                | "let_clause"
                | "join_clause"
                | "instanceof_expression"
        )
}

fn parameter_container(kind: &str) -> bool {
    kind.ends_with("parameters")
        || kind.ends_with("_parameter_list")
        || matches!(
            kind,
            "formal_parameters"
                | "parameter_list"
                | "parameters"
                | "inferred_parameters"
                | "lambda_parameters"
                | "method_parameters"
                | "block_parameters"
                | "function_value_parameters"
                | "formal_parameter_list"
                | "declArgs"
        )
}

fn parameters<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    // Some grammars put a signature beside its body. Its parameters belong to
    // the body scope, where sibling_parameters records them instead.
    if visit
        .node
        .parent()
        .is_some_and(|node| node.kind() == "function_signature")
    {
        return Ok(());
    }
    for parameter in named_children(visit.node) {
        context.ensure_active()?;
        let pattern = binding_pattern(parameter).unwrap_or(parameter);
        record(index, context, (visit, pattern, false))?;
    }
    Ok(())
}

fn sibling_parameters<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let Some(signature) = visit.node.prev_named_sibling() else {
        return Ok(());
    };
    let signature = named_children(signature)
        .find(|node| node.kind() == "function_signature")
        .unwrap_or(signature);
    let Some(parameters) = named_children(signature).find(|node| parameter_container(node.kind()))
    else {
        return Ok(());
    };
    for parameter in named_children(parameters) {
        context.ensure_active()?;
        record(
            index,
            context,
            (
                visit,
                binding_pattern(parameter).unwrap_or(parameter),
                false,
            ),
        )?;
    }
    Ok(())
}

fn binding_pattern(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "arrow_function" => node.child_by_field_name("parameter"),
        "lambda_expression" => node.child_by_field_name("parameters"),
        "parameter"
        | "formal_parameter"
        | "required_parameter"
        | "optional_parameter"
        | "parameter_declaration"
        | "class_parameter"
        | "lambda_parameter"
        | "parameter_with_optional_type"
        | "script_parameter"
        | "declArg"
        | "catch_formal_parameter"
        | "catch_declaration"
        | "catch_clause"
        | "declaration_pattern"
        | "var_pattern"
        | "in_clause"
        | "case_clause"
        | "match_case"
        | "match_arm" => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("pattern"))
            .or_else(|| node.child_by_field_name("declarator"))
            .or_else(|| node.child_by_field_name("parameter"))
            .or_else(|| named_children(node).find(|child| binding_leaf(child.kind()))),
        "variable_declarator" | "initialized_variable_definition" | "let_binding" => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("pattern")),
        _ => None,
    }
}

fn iteration_pattern(node: Node<'_>) -> Option<Node<'_>> {
    if !matches!(
        node.kind(),
        "for_in_statement"
            | "for_loop_parts"
            | "for_statement"
            | "enhanced_for_statement"
            | "foreach_statement"
            | "for_range_loop"
            | "for_expression"
            | "enumerator"
            | "for"
            | "foreach"
    ) {
        return None;
    }
    node.child_by_field_name("left")
        .or_else(|| node.child_by_field_name("name"))
        .or_else(|| node.child_by_field_name("pattern"))
        .or_else(|| node.child_by_field_name("item"))
        .or_else(|| node.child_by_field_name("iterator"))
        .or_else(|| {
            named_children(node)
                .find(|node| binding_leaf(node.kind()) || node.kind() == "variable_declaration")
        })
}

fn iteration_writes(node: Node<'_>) -> bool {
    if matches!(node.kind(), "for" | "foreach") {
        return true;
    }
    if !matches!(node.kind(), "for_in_statement" | "for_loop_parts") {
        return false;
    }
    let mut cursor = node.walk();
    !node.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "const"
                | "let"
                | "final_builtin"
                | "type_identifier"
                | "type"
                | "type_annotation"
                | "nullable_type"
        ) || child.kind() == "var" && node.kind() != "for_in_statement"
    })
}

fn write_pattern(node: Node<'_>) -> Option<Node<'_>> {
    if !matches!(
        node.kind(),
        "assignment_expression"
            | "assignment"
            | "operator_assignment"
            | "augmented_assignment_expression"
            | "update_expression"
            | "postfix_expression"
            | "prefix_expression"
            | "postfix_unary_expression"
            | "prefix_unary_expression"
            | "unary_expression"
            | "pre_increment_expression"
            | "pre_decrement_expression"
            | "post_increment_expression"
            | "post_decrement_expression"
    ) {
        return None;
    }
    if matches!(
        node.kind(),
        "prefix_expression"
            | "postfix_expression"
            | "prefix_unary_expression"
            | "postfix_unary_expression"
            | "unary_expression"
    ) && !increment(node)
    {
        return None;
    }
    node.child_by_field_name("left")
        .or_else(|| node.child_by_field_name("lhs"))
        .or_else(|| node.child_by_field_name("argument"))
        .or_else(|| {
            named_children(node).find(|node| {
                !matches!(
                    node.kind(),
                    "increment_operator" | "prefix_operator" | "postfix_operator"
                )
            })
        })
}

fn increment(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| {
        increment_token(child)
            || matches!(child.kind(), "prefix_operator" | "postfix_operator")
                && named_children(child).any(increment_token)
    })
}

fn increment_token(node: Node<'_>) -> bool {
    matches!(node.kind(), "++" | "--" | "increment_operator")
}

fn binding_leaf(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "constant"
            | "field_identifier"
            | "type_identifier"
            | "module_name"
            | "simple_identifier"
            | "value_name"
            | "value_pattern"
            | "variable"
            | "implicit_parameter"
            | "shorthand_property_identifier_pattern"
    )
}

fn record<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'tree>, Node<'tree>, bool),
) -> Result<(), ExtractError> {
    let (visit, pattern, write) = input;
    nominal::reserve_slot(context, &mut index.fallback_bindings)?;
    index.fallback_bindings.push(FallbackBinding {
        visit,
        pattern,
        write,
    });
    Ok(())
}

pub(super) fn finish(
    index: &mut SyntaxIndex<'_>,
    builder: &mut ExtractionBuilder<'_, '_>,
) -> Result<(), ExtractError> {
    let mut budget = AstVisitBudget::<{ super::super::MAX_AST_DEPTH }>::default();
    for binding in std::mem::take(&mut index.fallback_bindings) {
        builder.context.ensure_active()?;
        let mut pending = Vec::new();
        push_visit(
            &mut builder.context,
            &mut pending,
            Visit {
                node: binding.pattern,
                ..binding.visit
            },
        )?;
        while let Some(visit) = pending.pop() {
            budget.observe(builder, visit.depth)?;
            if binding_leaf(visit.node.kind()) {
                let write = binding.write.then_some((
                    binding.visit.node.end_byte(),
                    iteration_pattern(binding.visit.node).is_some(),
                ));
                bind_missing(index, &mut builder.context, (visit, write))?;
            } else if !pattern_children(&mut builder.context, &mut pending, visit)? {
                index.fence(&mut builder.context, visit.scope)?;
            }
        }
    }
    Ok(())
}

fn ambiguous_declaration(node: Node<'_>, language: SourceLanguage) -> Option<Node<'_>> {
    if language != SourceLanguage::Cpp
        || node.kind() != "call_expression"
        || node.parent()?.kind() != "expression_statement"
    {
        return None;
    }
    let head = node.child_by_field_name("function")?;
    let mut arguments = named_children(node.child_by_field_name("arguments")?);
    let name = arguments.next()?;
    // C++ parses T(value); as a call even when it declares a shadowing local.
    // Aliases and compound type heads are unmodeled, so preserve neither proof.
    (matches!(
        head.kind(),
        "identifier" | "qualified_identifier" | "template_function"
    ) && matches!(
        name.kind(),
        "identifier" | "parenthesized_expression" | "pointer_expression"
    ) && arguments.next().is_none())
    .then_some(name)
}

fn pattern_children<'tree>(
    context: &mut ExtractionContext<'_, '_>,
    pending: &mut Vec<Visit<'tree>>,
    visit: Visit<'tree>,
) -> Result<bool, ExtractError> {
    if let Some(pattern) = binding_pattern(visit.node).or_else(|| pattern_target(visit.node)) {
        push_visit(
            context,
            pending,
            Visit {
                node: pattern,
                depth: visit.depth.saturating_add(1),
                ..visit
            },
        )?;
        return Ok(true);
    }
    if !pattern_container(visit.node.kind()) {
        return Ok(empty_pattern(visit.node));
    }
    for child in named_children(visit.node) {
        if visit
            .node
            .child_by_field_name("type")
            .is_some_and(|node| node.id() == child.id())
        {
            continue;
        }
        push_visit(
            context,
            pending,
            Visit {
                node: child,
                depth: visit.depth.saturating_add(1),
                ..visit
            },
        )?;
    }
    Ok(true)
}

fn empty_pattern(node: Node<'_>) -> bool {
    let kind = node.kind();
    parameter_without_name(node)
        || kind.contains("modifier")
        || matches!(
            kind,
            "unit"
                | "wildcard_pattern"
                | "discard"
                | "primitive_type"
                | "type_identifier"
                | "void_type"
                | "type_qualifier"
                | "type_annotation"
                | "inferred_type"
                | "this"
                | "super"
                | "parameter_modifiers"
                | "modifiers"
                | "annotation"
                | "member_expression"
                | "field_expression"
                | "member_access_expression"
                | "navigation_expression"
                | "assignable_expression"
                | "field_access"
                | "element_reference"
                | "subscript_expression"
                | "element_access_expression"
                | "instance_variable"
                | "class_variable"
                | "global_variable"
        )
}

fn binding_child(node: Node<'_>) -> Option<Node<'_>> {
    let annotation = node.child_by_field_name("type");
    named_children(node).find(|child| {
        binding_leaf(child.kind()) && annotation.is_none_or(|node| node.id() != child.id())
    })
}

fn parameter_without_name(node: Node<'_>) -> bool {
    let Some(annotation) = node.child_by_field_name("type") else {
        return false;
    };
    node.kind().contains("parameter")
        && named_children(node)
            .all(|child| child.id() == annotation.id() || child.kind().contains("modifier"))
}

fn pattern_target(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "assignable_expression" => sole_named_child(node),
        "assignment_pattern" | "object_assignment_pattern" | "pair_pattern" => node
            .child_by_field_name("left")
            .or_else(|| node.child_by_field_name("value")),
        "pointer_declarator"
        | "reference_declarator"
        | "init_declarator"
        | "parenthesized_declarator"
        | "function_declarator"
        | "attributed_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| named_children(node).next()),
        "variable_declaration" | "pattern" => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("bound_identifier"))
            .or_else(|| named_children(node).find(|node| binding_leaf(node.kind()))),
        "formal_parameter" | "constructor_param" => named_children(node)
            .find(|node| binding_leaf(node.kind()) || node.kind() == "constructor_param"),
        _ if pattern_kind(node.kind()) => node
            .child_by_field_name("pattern")
            .or_else(|| node.child_by_field_name("bound_identifier"))
            .or_else(|| node.child_by_field_name("name")),
        _ if node.kind().contains("parameter") => binding_child(node),
        _ => None,
    }
}

fn sole_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut children = named_children(node);
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

fn pattern_container(kind: &str) -> bool {
    parameter_container(kind)
        || pattern_kind(kind)
        || matches!(
            kind,
            "object_pattern"
                | "multi_variable_declaration"
                | "left_assignment_list"
                | "destructured_left_assignment"
                | "rest_assignment"
                | "tuple_expression"
                | "argument"
                | "parenthesized_expression"
                | "array_pattern"
                | "tuple_pattern"
                | "rest_pattern"
                | "list_pattern"
                | "cons_pattern"
                | "as_pattern"
                | "record_pattern"
                | "structured_binding_declarator"
                | "left_assignment_expression"
                | "directly_assignable_expression"
                | "expression"
                | "logical_expression"
                | "bitwise_expression"
                | "comparison_expression"
                | "additive_expression"
                | "multiplicative_expression"
                | "format_expression"
                | "range_expression"
                | "array_literal_expression"
                | "unary_expression"
        )
}

fn bind_missing<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'tree>, Option<(usize, bool)>),
) -> Result<(), ExtractError> {
    let (visit, write) = input;
    let name = node_text(context, visit.node).trim_start_matches('$');
    if !identifier(name) {
        return Ok(());
    }
    if write.is_some_and(|(_, iteration)| !iteration)
        && visible_annotation(index, context, (visit.scope, name))?
    {
        return Ok(());
    }
    let key = nominal::binding_key(context, name)?;
    let existing = index
        .scopes
        .get_mut(&visit.scope)
        .and_then(|scope| scope.bindings.get_mut(key.as_ref()));
    if let Some(existing) = existing {
        if write.is_some_and(|(end, _)| existing.start != end)
            && matches!(
                existing.kind,
                BindingType::Nominal(_)
                    | BindingType::Import
                    | BindingType::Explicit {
                        origin: TypeOrigin::Constructor,
                        ..
                    }
            )
        {
            existing.kind = BindingType::Unknown;
        }
    } else {
        index.bind_name(
            context,
            NamedBind {
                scope: visit.scope,
                name,
                kind: BindingType::Unknown,
                start: 0,
            },
        )?;
    }
    if write.is_some() {
        nominal::bindings::invalidate_visible(index, context, (visit.scope, visit.node))?;
    }
    Ok(())
}

fn visible_annotation(
    index: &SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (usize, &str),
) -> Result<bool, ExtractError> {
    let (scope, name) = input;
    let types = index.types();
    let binding = types.find_binding(
        context,
        TypeQuery {
            scope,
            name,
            class_bindings: true,
            position: None,
        },
    )?;
    Ok(binding.is_some_and(|binding| {
        matches!(
            binding.kind,
            BindingType::Explicit {
                origin: TypeOrigin::Annotation,
                ..
            }
        )
    }))
}
