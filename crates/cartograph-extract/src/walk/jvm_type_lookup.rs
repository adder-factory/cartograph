//! Keep v1's visible type segments while retaining syntax-proven JVM lookup.
//!
//! `Result.Success` still emits `Result` and `Success`; the latter looks up
//! `Result.Success`, so the resolver can follow an import of `Result` without
//! guessing by the globally unique short name. A lexical type parameter never
//! names a same-spelled nominal class.

use cartograph_domain::{ReferenceKind, SourceLanguage};
use tree_sitter::Node;

use crate::{DeclarationSyntax, ExtractError, ExtractedReference};

use super::{
    ExtractionBuilder, PendingReference,
    managed_value_lookup::{BoundedNode, bounded_named_child},
    references,
    syntax::{named_children, span_for},
};

const MAX_SCOPE_HOPS: usize = 64;
const MAX_LOOKUP_BYTES: usize = 512;
const TYPE_PARAMETER_PREFIX: &str = "native-jvm-type-parameter::";

pub(super) fn declaration_syntax(input: (SourceLanguage, Node<'_>)) -> DeclarationSyntax {
    let (language, node) = input;
    if language == SourceLanguage::Kotlin
        && matches!(node.kind(), "primary_constructor" | "secondary_constructor")
    {
        DeclarationSyntax::KotlinConstructor
    } else {
        DeclarationSyntax::Other
    }
}

pub(super) fn record_java_local_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Java
        || !matches!(node.kind(), "class_declaration" | "record_declaration")
    {
        return Ok(());
    }
    let Some(block) = node.parent().filter(|parent| parent.kind() == "block") else {
        return Ok(());
    };
    let bytes = u64::try_from(size_of::<(
        cartograph_domain::SourceSpan,
        cartograph_domain::SourceSpan,
    )>())
    .map_err(|_| ExtractError::OutputLimit)?
    .saturating_mul(2);
    builder
        .context
        .budget
        .reserve_fact(bytes, std::iter::empty())?;
    builder
        .facts
        .local_type_scopes
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder
        .facts
        .local_type_scopes
        .push((span_for(node)?, span_for(block)?));
    Ok(())
}

pub(super) fn push_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: PendingReference<'_>,
) -> Result<(), ExtractError> {
    if super::managed_value_lookup::shadows_receiver(builder, &pending)? {
        record_abstention(builder, pending.node)?;
    }
    let lookup = type_lookup(builder, &pending)?;
    let Some(resolution_name) = lookup else {
        return references::push_reference(builder, pending);
    };
    builder.emit_reference(ExtractedReference {
        owner: pending.owner,
        name: pending.name,
        resolution_name: Some(resolution_name),
        kind: pending.kind,
        span: span_for(pending.node)?,
    })
}

pub(super) fn guard_kotlin_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "object_declaration" => record_abstention(builder, node),
        "companion_object" => guard_inherited_companion(builder, node),
        "function_declaration" => guard_operator_invocation(builder, node),
        _ => Ok(()),
    }
}

fn guard_inherited_companion(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if matches!(
        bounded_named_child(builder, (node, &["delegation_specifier"]))?,
        BoundedNode::Absent
    ) {
        return Ok(());
    }
    guard_companion_owner(builder, node)
}

fn guard_companion_owner(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let mut parent = node.parent();
    for _ in 0..MAX_SCOPE_HOPS {
        builder.context.ensure_active()?;
        let Some(node) = parent else {
            return Ok(());
        };
        if node.kind() == "class_declaration" {
            return record_abstention(builder, node);
        }
        parent = node.parent();
    }
    Err(ExtractError::OutputLimit)
}

fn guard_operator_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !operator_invoke(builder, node)? {
        return Ok(());
    }
    let mut parent = node.parent();
    for _ in 0..MAX_SCOPE_HOPS {
        builder.context.ensure_active()?;
        let Some(node) = parent else {
            return Ok(());
        };
        match node.kind() {
            "companion_object" => return guard_companion_owner(builder, node),
            "object_declaration" | "class_declaration" => return Ok(()),
            _ => parent = node.parent(),
        }
    }
    Err(ExtractError::OutputLimit)
}

fn operator_invoke(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let BoundedNode::Found(name) = bounded_named_child(builder, (node, &["simple_identifier"]))?
    else {
        return Ok(false);
    };
    if builder.context.text(name) != "invoke" {
        return Ok(false);
    }
    let BoundedNode::Found(modifiers) = bounded_named_child(builder, (node, &["modifiers"]))?
    else {
        return Ok(false);
    };
    for (position, modifier) in named_children(modifiers).enumerate() {
        builder.context.ensure_active()?;
        if position >= MAX_SCOPE_HOPS {
            return Err(ExtractError::OutputLimit);
        }
        if modifier.kind() == "function_modifier" && builder.context.text(modifier) == "operator" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn record_abstention(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let bytes = u64::try_from(size_of::<cartograph_domain::SourceSpan>())
        .map_err(|_| ExtractError::OutputLimit)?
        .saturating_mul(2);
    builder
        .context
        .budget
        .reserve_fact(bytes, std::iter::empty())?;
    builder
        .facts
        .resolution_abstentions
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.facts.resolution_abstentions.push(span_for(node)?);
    Ok(())
}

fn type_lookup(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<Option<String>, ExtractError> {
    let language = builder.context.snapshot.language();
    if !(matches!(language, SourceLanguage::Java | SourceLanguage::Kotlin)
        || (language == SourceLanguage::CSharp && pending.kind == ReferenceKind::Calls))
        || !matches!(
            pending.kind,
            ReferenceKind::TypeOf
                | ReferenceKind::Returns
                | ReferenceKind::Extends
                | ReferenceKind::Implements
                | ReferenceKind::Inherits
                | ReferenceKind::Calls
                | ReferenceKind::Instantiates
        )
    {
        return Ok(None);
    }
    let head = pending.name.split('.').next().unwrap_or(&pending.name);
    if lexical_type_parameter(builder, (pending.node, head))? {
        return builder
            .context
            .copy_text(&format!("{TYPE_PARAMETER_PREFIX}{}", pending.name))
            .map(Some);
    }
    if language == SourceLanguage::Kotlin {
        let lookup = kotlin_path(builder, pending.node)?;
        if let Some(lookup) = lookup.as_deref() {
            let head = lookup.split('.').next().unwrap_or(lookup);
            if lexical_type_parameter(builder, (pending.node, head))? {
                return builder
                    .context
                    .copy_text(&format!("{TYPE_PARAMETER_PREFIX}{lookup}"))
                    .map(Some);
            }
        }
        return Ok(lookup);
    }
    Ok(None)
}

fn lexical_type_parameter(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: (Node<'_>, &str),
) -> Result<bool, ExtractError> {
    let (node, name) = input;
    let mut scope = node.parent();
    for _ in 0..MAX_SCOPE_HOPS {
        builder.context.ensure_active()?;
        let Some(node) = scope else {
            return Ok(false);
        };
        if declares_type_parameter(builder, node, name)? {
            return Ok(true);
        }
        scope = node.parent();
    }
    // An enclosing declaration beyond the bounded scan can shadow the name.
    Ok(true)
}

fn declares_type_parameter(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    if !matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "method_declaration"
            | "function_declaration"
            | "constructor_declaration"
            | "local_function_statement"
            | "struct_declaration"
    ) {
        return Ok(false);
    }
    let parameters = match node.child_by_field_name("type_parameters") {
        Some(parameters) => BoundedNode::Found(parameters),
        None => bounded_named_child(builder, (node, &["type_parameters", "type_parameter_list"]))?,
    };
    let parameters = match parameters {
        BoundedNode::Found(parameters) => parameters,
        BoundedNode::Absent => return Ok(false),
        BoundedNode::Truncated => return Ok(true),
    };
    for (position, parameter) in named_children(parameters).enumerate() {
        builder.context.ensure_active()?;
        if position >= MAX_SCOPE_HOPS {
            return Ok(true);
        }
        let declared = match parameter.child_by_field_name("name") {
            Some(declared) => BoundedNode::Found(declared),
            None => bounded_named_child(builder, (parameter, &["type_identifier", "identifier"]))?,
        };
        match declared {
            BoundedNode::Truncated => return Ok(true),
            BoundedNode::Found(declared) if builder.context.text(declared) == name => {
                return Ok(true);
            }
            BoundedNode::Absent | BoundedNode::Found(_) => {}
        }
    }
    Ok(false)
}

fn kotlin_path(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(path) = node.parent().filter(|parent| parent.kind() == "user_type") else {
        return Ok(None);
    };
    let mut lookup = String::new();
    for segment in named_children(path).filter(|child| child.kind() == "type_identifier") {
        builder.context.ensure_active()?;
        if segment.start_byte() > node.start_byte() {
            break;
        }
        let name = builder.context.text(segment);
        let bytes = lookup.len().saturating_add(name.len()).saturating_add(1);
        if bytes > MAX_LOOKUP_BYTES {
            return Err(ExtractError::OutputLimit);
        }
        builder.context.budget.ensure_string_length(bytes)?;
        lookup
            .try_reserve(name.len().saturating_add(1))
            .map_err(|_| ExtractError::OutputLimit)?;
        if !lookup.is_empty() {
            lookup.push('.');
        }
        lookup.push_str(name);
    }
    if !lookup.contains('.') {
        return Ok(None);
    }
    builder.context.copy_text(&lookup).map(Some)
}
