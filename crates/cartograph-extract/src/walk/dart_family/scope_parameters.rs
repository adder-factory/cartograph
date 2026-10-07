//! Parameter bindings participate in single-step nominal receiver proof.

use super::super::current_class_calls::unshadowed_receiver;
use super::ExtractionBuilder;
use crate::ExtractError;
use tree_sitter::Node;

pub(in super::super) fn unshadowed_receiver_parameters(
    builder: &mut ExtractionBuilder<'_, '_>,
    signature: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    let Some((inner, _)) = super::named_signature(signature) else {
        return Ok(false);
    };
    match super::named_child_of_kind(inner, "formal_parameter_list") {
        Some(parameters) => unshadowed_receiver(builder, parameters, name),
        None => Ok(true),
    }
}
