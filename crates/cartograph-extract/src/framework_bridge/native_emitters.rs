//! Only imported native emitters with an unmodified, in-scope binding own subscriptions.

use tree_sitter::Node;

use super::{
    BTreeMap, ExtractError, FrameworkBuilder, alias_shapes, javascript_bindings, javascript_shapes,
};
use javascript_bindings::{Imports, NativeImport};

const MAX_SCOPE_HOPS: usize = 16;
const BINDING_BYTES: u64 = 256;

pub(super) struct Binding {
    scope_start: usize,
    scope_end: usize,
    ready_at: usize,
}

pub(super) type Emitters = BTreeMap<String, Binding>;

pub(super) fn collect(
    builder: &mut FrameworkBuilder<'_, '_>,
    (imports, unique): (&Imports, &BTreeMap<String, usize>),
) -> Result<Emitters, ExtractError> {
    let mut emitters = Emitters::new();
    let Some(root) = builder.syntax_root() else {
        return Ok(emitters);
    };
    let mut cursor = root.walk();
    loop {
        builder.bridge.charge_work(1)?;
        if let Some((name, binding)) = declaration(builder, (cursor.node(), imports, unique)) {
            builder.bridge.reserve_working_bytes(
                BINDING_BYTES + u64::try_from(name.len()).map_err(|_| ExtractError::OutputLimit)?,
            )?;
            emitters.insert(name.to_owned(), binding);
        }
        if !alias_shapes::advance(builder, &mut cursor)? {
            return Ok(emitters);
        }
    }
}

fn declaration<'source>(
    builder: &FrameworkBuilder<'source, '_>,
    (node, imports, unique): (Node<'_>, &Imports, &BTreeMap<String, usize>),
) -> Option<(&'source str, Binding)> {
    if node.kind() != "variable_declarator" || node.parent()?.kind() != "lexical_declaration" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    let name = builder.source().get(name.byte_range())?;
    if unique.get(name) != Some(&1) {
        return None;
    }
    let value = javascript_shapes::unwrap(node.child_by_field_name("value")?)?;
    if value.kind() != "new_expression" {
        return None;
    }
    let constructor = value.child_by_field_name("constructor")?;
    if constructor.kind() != "identifier"
        || imports.get(builder.source().get(constructor.byte_range())?)
            != Some(&NativeImport::Emitter)
    {
        return None;
    }
    scope(node).map(|(scope_start, scope_end)| {
        (
            name,
            Binding {
                scope_start,
                scope_end,
                ready_at: value.end_byte(),
            },
        )
    })
}

fn scope(mut node: Node<'_>) -> Option<(usize, usize)> {
    for _ in 0..MAX_SCOPE_HOPS {
        if matches!(
            node.kind(),
            "for_statement" | "for_in_statement" | "switch_body"
        ) {
            return None;
        }
        if matches!(node.kind(), "program" | "statement_block") {
            return Some((node.start_byte(), node.end_byte()));
        }
        node = node.parent()?;
    }
    None
}

pub(super) fn accepts(
    source: &str,
    (receiver, call): (Node<'_>, Node<'_>),
    (imports, emitters): (&Imports, &Emitters),
) -> bool {
    if receiver.kind() != "identifier" {
        return false;
    }
    let Some(name) = source.get(receiver.byte_range()) else {
        return false;
    };
    imports.get(name) == Some(&NativeImport::DeviceEmitter)
        || emitters.get(name).is_some_and(|binding| {
            binding.scope_start <= call.start_byte()
                && binding.ready_at <= call.start_byte()
                && call.end_byte() <= binding.scope_end
        })
}
