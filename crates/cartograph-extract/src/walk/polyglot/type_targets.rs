//! Possibly qualified reference targets and declared-type capture for the
//! Python, Go, and Rust walkers.
//!
//! A target is recorded under its leaf name (`Base` for `model.Base`), and a
//! qualified path is kept as the resolution name (see [`super::qualified_path`]),
//! so the resolver looks it up through its import binding or module path
//! rather than by a bare leaf that a same-named local declaration could claim.
//! A qualified target without a lookup name is omitted.

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId};
use tree_sitter::Node;

use super::{
    import_index::{TypingForm, local_imports},
    qualified_path,
};
use crate::{
    ExtractError, ExtractedReference,
    walk::{
        ExtractionBuilder,
        syntax::{named_children, span_for},
    },
};

/// Deepest type expression descended while capturing declared types; deeper
/// type names are omitted, never failing the file.
const MAX_DECLARED_TYPE_DEPTH: usize = 64;

/// A possibly qualified target: `path` is the whole written target
/// (`model.Base`, `m::P`) and `leaf` its final name segment.
#[derive(Clone, Copy)]
pub(super) struct NamedTarget<'tree> {
    pub(super) path: Node<'tree>,
    pub(super) leaf: Node<'tree>,
}

impl<'tree> NamedTarget<'tree> {
    /// A target written as a single name.
    pub(super) const fn unqualified(node: Node<'tree>) -> Self {
        Self {
            path: node,
            leaf: node,
        }
    }

    /// This target used through a `kind` relationship.
    pub(super) const fn reference(self, kind: ReferenceKind) -> LeafReference<'tree> {
        LeafReference { target: self, kind }
    }
}

/// One relationship to a [`NamedTarget`].
#[derive(Clone, Copy)]
pub(super) struct LeafReference<'tree> {
    target: NamedTarget<'tree>,
    kind: ReferenceKind,
}

/// One type expression whose named types are used by each of `owners`.
#[derive(Clone, Copy)]
struct DeclaredType<'tree, 'owner> {
    node: Node<'tree>,
    owners: &'owner [SymbolId],
    depth: usize,
}

/// Record `reference`, used by `owner`, under its target's leaf name, keeping a
/// qualified path as the resolution name.
pub(super) fn emit_leaf_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    owner: Option<SymbolId>,
    reference: LeafReference<'_>,
) -> Result<(), ExtractError> {
    match leaf_reference(builder, reference)? {
        Some(reference) => builder.emit_reference(ExtractedReference { owner, ..reference }),
        None => Ok(()),
    }
}

/// `reference` as an unowned extracted reference, or `None` when its target
/// has no name or a qualified target has no lookup name.
fn leaf_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    reference: LeafReference<'_>,
) -> Result<Option<ExtractedReference>, ExtractError> {
    let NamedTarget { path, leaf } = reference.target;
    let resolution_name = if path.id() == leaf.id() {
        None
    } else {
        let Some(path) = qualified_path::lookup_name(builder, path)? else {
            return Ok(None);
        };
        Some(path)
    };
    let name = builder.context.owned_text(leaf)?;
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(ExtractedReference {
        owner: None,
        name,
        resolution_name,
        kind: reference.kind,
        span: span_for(leaf)?,
    }))
}

/// Record one written type target as a use by each of `owners`. The target's
/// names are built once, however many owners share it.
fn emit_shared_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    owners: &[SymbolId],
    reference: LeafReference<'_>,
) -> Result<(), ExtractError> {
    let Some(shared) = leaf_reference(builder, reference)? else {
        return Ok(());
    };
    for owner in owners {
        let resolution_name = match shared.resolution_name.as_deref() {
            Some(path) => Some(builder.context.copy_text(path)?),
            None => None,
        };
        builder.emit_reference(ExtractedReference {
            owner: Some(owner.clone()),
            name: builder.context.copy_text(&shared.name)?,
            resolution_name,
            kind: shared.kind,
            span: shared.span,
        })?;
    }
    Ok(())
}

/// Record each type named in a declared type (`map[string]*pkg.User`,
/// `Vec<m::Item>`, `list[models.User]`) as a `TypeOf` use by `owner`, once
/// per written occurrence and with its qualifier kept for resolution.
pub(super) fn capture_declared_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    capture_shared_declared_types(builder, root, std::slice::from_ref(owner))
}

/// Record each type named in a declared type that several bindings share
/// (`var A, B pkg.T`) as a `TypeOf` use by each of them. The type is
/// traversed once, however many bindings share it.
pub(super) fn capture_shared_declared_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    owners: &[SymbolId],
) -> Result<(), ExtractError> {
    if owners.is_empty() {
        return Ok(());
    }
    capture_declared_type(
        builder,
        DeclaredType {
            node: root,
            owners,
            depth: 0,
        },
    )
}

fn capture_declared_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: DeclaredType<'_, '_>,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    if input.depth > MAX_DECLARED_TYPE_DEPTH {
        return Ok(());
    }
    let language = builder.context.snapshot.language();
    if let Some(target) = declared_type_target(language, input.node) {
        return emit_shared_reference(
            builder,
            input.owners,
            target.reference(ReferenceKind::TypeOf),
        );
    }
    match (language, input.node.kind()) {
        // `Iterator<Item = T>` names the associated type `Item`, which is no use.
        (SourceLanguage::Rust, "type_binding") => input
            .node
            .child_by_field_name("type")
            .map_or(Ok(()), |node| capture_nested_type(builder, input, node)),
        // A call or string in an annotation (`Annotated[T, Tag()]`) is a value.
        (SourceLanguage::Python, "call" | "string" | "concatenated_string") => Ok(()),
        (SourceLanguage::Python, "generic_type" | "subscript") => {
            capture_python_subscript(builder, input)
        }
        _ => {
            for node in named_children(input.node) {
                capture_nested_type(builder, input, node)?;
            }
            Ok(())
        }
    }
}

/// A subscripted Python annotation: its head, then only its type-bearing
/// arguments (`typing.Literal[..]` lists values; `typing.Annotated[T, ..]`
/// attaches metadata after `T`).
fn capture_python_subscript(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: DeclaredType<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(head) = named_children(input.node).find(|child| !child.is_extra()) else {
        return Ok(());
    };
    let type_arguments =
        python_typing_form(builder, head)?.map_or(usize::MAX, TypingForm::type_arguments);
    capture_nested_type(builder, input, head)?;
    // `X[a, b]` lists its arguments directly (subscript) or in a parameter list (generic type).
    let arguments = named_children(input.node)
        .filter(|child| child.id() != head.id())
        .flat_map(|child| {
            let listed = child.kind() == "type_parameter";
            named_children(child)
                .filter(move |_| listed)
                .chain(std::iter::once(child).filter(move |_| !listed))
        })
        .filter(|argument| !argument.is_extra());
    for argument in arguments.take(type_arguments) {
        capture_nested_type(builder, input, argument)?;
    }
    Ok(())
}

/// The `typing` special form a subscript head names through the file's
/// imports: `Literal` from `from typing import Literal` (or an alias of it),
/// or `t.Annotated` with `import typing as t`. A same-named local declaration
/// is no special form. A name any typing import binds is read as that form
/// (see [`super::import_index`]), so value arguments never become types.
fn python_typing_form(
    builder: &mut ExtractionBuilder<'_, '_>,
    head: Node<'_>,
) -> Result<Option<TypingForm>, ExtractError> {
    let (local, member) = match head.kind() {
        "identifier" => (head, None),
        "attribute" => {
            let Some((object, member)) = head
                .child_by_field_name("object")
                .filter(|object| object.kind() == "identifier")
                .zip(head.child_by_field_name("attribute"))
            else {
                return Ok(None);
            };
            (object, Some(member))
        }
        _ => return Ok(None),
    };
    let snapshot = builder.context.snapshot;
    let source = snapshot.source();
    let text = |node: Node<'_>| {
        source
            .get(node.start_byte()..node.end_byte())
            .unwrap_or_default()
            .trim()
    };
    let Some(imports) = local_imports(builder, text(local))? else {
        return Ok(None);
    };
    Ok(match member {
        None => imports.typing_form,
        Some(member) if imports.typing_namespace => TypingForm::named(text(member)),
        Some(_) => None,
    })
}

fn capture_nested_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    parent: DeclaredType<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    capture_declared_type(
        builder,
        DeclaredType {
            node,
            owners: parent.owners,
            depth: parent.depth.saturating_add(1),
        },
    )
}

/// The named type a type-position node denotes, when it is a complete name.
fn declared_type_target(language: SourceLanguage, node: Node<'_>) -> Option<NamedTarget<'_>> {
    match (language, node.kind()) {
        (SourceLanguage::Python, "identifier") | (_, "type_identifier") => {
            Some(NamedTarget::unqualified(node))
        }
        (SourceLanguage::Go, "qualified_type")
        | (SourceLanguage::Rust, "scoped_type_identifier") => Some(NamedTarget {
            path: node,
            leaf: node.child_by_field_name("name")?,
        }),
        (SourceLanguage::Python, "attribute") => Some(NamedTarget {
            path: node,
            leaf: node.child_by_field_name("attribute")?,
        }),
        _ => None,
    }
}
