//! One pass over `CodeIgniter` resource receivers; conflicting aliases abstain.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::ReferenceKind;
use tree_sitter::Node;

use super::{LoadedResource, ci_class_name, identifier_at, loads::LoadedSet, property_bindings};
use crate::{
    ExtractError,
    framework::{
        FrameworkBuilder, FrameworkNearReferenceInput, FrameworkReferenceInput,
        skip_ascii_whitespace, syntax_nodes::SyntaxNodes,
    },
};

const RESOURCE_INDEX_ENTRY_BYTES: u64 = 192;
const INFERRED_RESOURCE_PREFIX: &str = "ci-inferred::";

struct CallPolicy<'resource> {
    aliases: BTreeMap<&'resource str, Option<&'resource LoadedResource>>,
    properties: BTreeSet<String>,
}

pub(super) fn add_reference(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: FrameworkNearReferenceInput<'_>,
) -> Result<(), ExtractError> {
    if controller_file(builder) {
        return builder.add_reference_near_with_resolution(input);
    }
    builder.add_reference(FrameworkReferenceInput {
        owner: None,
        name: input.name,
        resolution_name: input.resolution_name,
        kind: input.kind,
        start: input.start,
        end: input.end,
    })
}

pub(super) fn scan_calls(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    loaded: &LoadedSet,
) -> Result<(), ExtractError> {
    if !loaded.bindings_complete {
        return Ok(());
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(());
    };
    if !single_receiver_context(builder, root)? {
        return Ok(());
    }
    let Some(properties) = property_bindings::scan(builder, source, root)? else {
        return Ok(());
    };
    let policy = CallPolicy {
        aliases: alias_index(builder, &loaded.resources)?,
        properties,
    };
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        let offset = node.start_byte();
        if node.kind() != "member_call_expression" || !source[offset..].starts_with("$this->") {
            continue;
        }
        let Some(call) = member_call(source, offset + "$this->".len()) else {
            continue;
        };
        let Some(resolution) = call_resolution(&policy, &call) else {
            continue;
        };
        add_reference(
            builder,
            FrameworkNearReferenceInput {
                name: call.method,
                resolution_name: Some(&resolution),
                kind: ReferenceKind::Calls,
                start: call.start,
                end: call.end,
            },
        )?;
    }
    Ok(())
}

fn single_receiver_context(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut found = false;
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        let kind = node.kind();
        if matches!(
            kind,
            "anonymous_class" | "function_definition" | "anonymous_function" | "arrow_function"
        ) {
            return Ok(false);
        }
        if !matches!(
            kind,
            "class_declaration"
                | "trait_declaration"
                | "interface_declaration"
                | "enum_declaration"
        ) {
            continue;
        }
        if found {
            return Ok(false);
        }
        found = true;
    }
    Ok(found)
}

fn alias_index<'resource>(
    builder: &mut FrameworkBuilder<'_, '_>,
    resources: &'resource [LoadedResource],
) -> Result<BTreeMap<&'resource str, Option<&'resource LoadedResource>>, ExtractError> {
    let bytes = resources.iter().try_fold(0_u64, |total, resource| {
        let lengths =
            u64::try_from(resource.alias.len() + resource.class.len() + resource.path.len())
                .map_err(|_| ExtractError::OutputLimit)?;
        Ok::<_, ExtractError>(
            total
                .saturating_add(RESOURCE_INDEX_ENTRY_BYTES)
                .saturating_add(lengths),
        )
    })?;
    builder.bridge.reserve_working_bytes(bytes)?;
    let mut aliases = BTreeMap::new();
    for resource in resources {
        builder.bridge.charge_work(1)?;
        let entry = aliases
            .entry(resource.alias.as_str())
            .or_insert(Some(resource));
        if entry.is_some_and(|known| known.path != resource.path || known.kind != resource.kind) {
            *entry = None;
        }
    }
    Ok(aliases)
}

fn call_resolution(policy: &CallPolicy<'_>, call: &MemberCall<'_>) -> Option<String> {
    if policy.properties.contains(call.property) {
        return None;
    }
    match policy.aliases.get(call.property) {
        Some(Some(resource)) => Some(format!(
            "ci-loaded::{}::{}::{}",
            resource.kind, resource.path, call.method
        )),
        Some(None) => None,
        None => Some(format!(
            "{INFERRED_RESOURCE_PREFIX}{}::{}::{}",
            inferred_kind(call.property)?,
            ci_class_name(call.property),
            call.method
        )),
    }
}

pub(super) fn controller_file(builder: &FrameworkBuilder<'_, '_>) -> bool {
    builder
        .path()
        .to_ascii_lowercase()
        .starts_with("application/controllers/")
}

struct MemberCall<'s> {
    property: &'s str,
    method: &'s str,
    start: usize,
    end: usize,
}

fn member_call(source: &str, start: usize) -> Option<MemberCall<'_>> {
    let (property_end, property) = identifier_at(source, start)?;
    let arrow = skip_ascii_whitespace(source, property_end);
    if !source[arrow..].starts_with("->") {
        return None;
    }
    let start = skip_ascii_whitespace(source, arrow + "->".len());
    let (end, method) = identifier_at(source, start)?;
    (source.as_bytes().get(skip_ascii_whitespace(source, end)) == Some(&b'(')).then_some(
        MemberCall {
            property,
            method,
            start,
            end,
        },
    )
}

fn inferred_kind(property: &str) -> Option<&'static str> {
    if !property.contains('_') && !property.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    Some(if property.to_ascii_lowercase().ends_with("_model") {
        "model"
    } else {
        "library"
    })
}
