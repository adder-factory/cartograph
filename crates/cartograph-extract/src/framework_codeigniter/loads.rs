//! Executable `CodeIgniter` loads with complete literal resource/alias operands.

use cartograph_domain::ReferenceKind;
use tree_sitter::Node;

use super::{LoadedResource, Quoted, quoted_after, resources, screened_loaded_resource};
use crate::{
    ExtractError,
    framework::{
        FrameworkBuilder, FrameworkNearReferenceInput, skip_ascii_whitespace,
        syntax_nodes::SyntaxNodes,
    },
};

const MAX_LOADED_RESOURCES: usize = 256;
const MAX_LOAD_BYTES: usize = 4_096;
const RESOURCE_VECTOR_BYTES: u64 = 192;
const LIBRARY_PARAMETERS_INDEX: usize = 1;

pub(super) struct LoadedSet {
    pub(super) resources: Vec<LoadedResource>,
    pub(super) bindings_complete: bool,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<LoadedSet, ExtractError> {
    let mut loaded = LoadedSet {
        resources: Vec::new(),
        bindings_complete: true,
    };
    let Some(root) = builder.syntax_root().filter(|root| !root.has_error()) else {
        loaded.bindings_complete = false;
        return Ok(loaded);
    };
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        let Some((arguments, kind)) = load_call(source, node) else {
            continue;
        };
        let bytes = arguments.end_byte() - arguments.start_byte();
        if bytes > MAX_LOAD_BYTES {
            loaded.bindings_complete = false;
            continue;
        }
        builder.bridge.charge_work(bytes)?;
        let Some((resource, binding)) = load_operands((source, arguments), kind) else {
            loaded.bindings_complete = false;
            continue;
        };
        if loaded.resources.len() == MAX_LOADED_RESOURCES {
            loaded.bindings_complete = false;
            break;
        }
        record(builder, &mut loaded, (resource, binding))?;
    }
    Ok(loaded)
}

fn load_call<'tree>(source: &str, node: Node<'tree>) -> Option<(Node<'tree>, &'static str)> {
    if node.kind() != "member_call_expression" {
        return None;
    }
    let (open, kind) = load_start(source, node.start_byte())?;
    let arguments = node.child_by_field_name("arguments")?;
    (arguments.start_byte() == open).then_some((arguments, kind))
}

fn load_start(source: &str, offset: usize) -> Option<(usize, &'static str)> {
    let suffix = source.get(offset..)?.strip_prefix("$this->load->")?;
    let (name, kind) = if suffix.starts_with("model") {
        ("model", "model")
    } else {
        ("library", "library")
    };
    suffix.strip_prefix(name)?;
    let open = skip_ascii_whitespace(source, offset + "$this->load->".len() + name.len());
    (source.as_bytes().get(open) == Some(&b'(')).then_some((open, kind))
}

fn load_operands<'s>(
    input: (&'s str, Node<'_>),
    kind: &'static str,
) -> Option<(Quoted<'s>, LoadedResource)> {
    let (source, arguments) = input;
    let mut cursor = arguments.walk();
    let mut arguments = arguments
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "argument");
    let resource_node = arguments.next()?;
    let resource_text = source.get(resource_node.start_byte()..resource_node.end_byte())?;
    let mut resource = literal(resource_text)?;
    resource.start += resource_node.start_byte();
    resource.end += resource_node.start_byte();
    resource.quote_end += resource_node.start_byte();
    let alias_argument = if kind == "library" {
        arguments.nth(LIBRARY_PARAMETERS_INDEX)
    } else {
        arguments.next()
    };
    let alias = match alias_argument {
        Some(node) => Some(literal(source.get(node.start_byte()..node.end_byte())?)?),
        None => None,
    };
    let binding = screened_loaded_resource(&resource, alias, kind)?;
    Some((resource, binding))
}

fn literal(text: &str) -> Option<Quoted<'_>> {
    let first = skip_ascii_whitespace(text, 0);
    let quoted = quoted_after(text, first, text.len())?;
    (quoted.start == first + 1
        && skip_ascii_whitespace(text, quoted.quote_end + 1) == text.len()
        && !(text.as_bytes().get(first) == Some(&b'"') && quoted.value.contains('$')))
    .then_some(quoted)
}

fn record(
    builder: &mut FrameworkBuilder<'_, '_>,
    loaded: &mut LoadedSet,
    input: (Quoted<'_>, LoadedResource),
) -> Result<(), ExtractError> {
    let (resource, binding) = input;
    builder.bridge.reserve_working_bytes(
        RESOURCE_VECTOR_BYTES.saturating_add(
            u64::try_from(binding.alias.len() + binding.class.len() + binding.path.len())
                .map_err(|_| ExtractError::OutputLimit)?,
        ),
    )?;
    loaded
        .resources
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    let lookup = format!("ci-loaded::{}::{}", binding.kind, binding.path);
    resources::add_reference(
        builder,
        FrameworkNearReferenceInput {
            name: resource.value,
            resolution_name: Some(&lookup),
            kind: ReferenceKind::References,
            start: resource.start,
            end: resource.end,
        },
    )?;
    loaded.resources.push(binding);
    Ok(())
}
