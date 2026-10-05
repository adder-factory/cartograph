//! ABAP declarations, following the v1.1.33 implementation-first model.
//!
//! An ABAP class is split into a `DEFINITION` block, which lists its members
//! and their visibility, and an `IMPLEMENTATION` block, which holds the method
//! bodies. v1.1.33 declared a class and its methods at the implementation, so
//! the symbol spans cover the code that runs and calls inside a method body
//! belong to that method. When a file implements a class it also defines, the
//! one class symbol spans the implementation, takes its modifiers and doc
//! comment from the definition, and owns the definition's attributes and any
//! method the implementation does not implement (an abstract or deferred
//! method) as a body-less method. A definition without an implementation in
//! the file is declared on its own, with its methods body-less. Interfaces
//! declare their method signatures, and every other declaration (`DATA`
//! variables, field symbols, structures) follows the generic rules.
//!
//! The grammar cannot parse an interface-qualified method name
//! (`METHOD zif_greeter~greet.`): it names the method by the interface and
//! leaves `~greet` as an error node. v1.1.33 kept one method per implementation
//! under that name, so two such implementations are two symbols rather than
//! one symbol that swallows the second body.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::SymbolKind;
use tree_sitter::Node;

use super::{GenericDeclaration, emit_declaration, normalize_name, pop_scope, push_scope};
use crate::{
    ExtractError,
    walk::{ExtractionBuilder, syntax::named_children},
};

/// Grammar node of a class `DEFINITION` block.
const CLASS_DEFINITION_KIND: &str = "class_declaration";
/// Grammar node of a class `IMPLEMENTATION` block.
const CLASS_IMPLEMENTATION_KIND: &str = "class_implementation";
/// Grammar node of a `METHOD ... ENDMETHOD` block.
const METHOD_IMPLEMENTATION_KIND: &str = "method_implementation";
/// Grammar node holding a method implementation's statements; it is a child,
/// not a `body` field.
const METHOD_BODY_KIND: &str = "method_body";
/// Grammar node of an `INTERFACE ... ENDINTERFACE` block.
const INTERFACE_KIND: &str = "interface_declaration";
/// Suffix of the `PUBLIC` / `PROTECTED` / `PRIVATE SECTION` nodes of a definition.
const SECTION_KIND_SUFFIX: &str = "_section";
/// Member method declarations named by the keyword itself, because the
/// grammar gives them no name field.
const KEYWORD_METHOD_DECLARATIONS: [(&str, &str); 2] = [
    ("constructor_declaration", "constructor"),
    ("class_constructor_declaration", "class_constructor"),
];
/// Member method declarations named by their `name` field.
const NAMED_METHOD_DECLARATIONS: [&str; 2] = ["method_declaration", "class_method_declaration"];

/// Walk an ABAP compilation unit. A class definition whose implementation is
/// in the same file is walked once, inside the first such implementation's
/// class scope, so it declares no second class symbol.
pub(super) fn visit_program(
    builder: &mut ExtractionBuilder<'_, '_>,
    program: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let mut definitions = implemented_definitions(builder, program)?;
    let merged = definitions.values().map(Node::id).collect::<BTreeSet<_>>();
    let child_depth = depth.saturating_add(1);
    for child in named_children(program) {
        builder.context.ensure_active()?;
        if merged.contains(&child.id()) {
            continue;
        }
        if child.kind() != CLASS_IMPLEMENTATION_KIND {
            builder.visit(child, child_depth)?;
            continue;
        }
        let definition = class_key(builder, child)?.and_then(|key| definitions.remove(&key));
        visit_class_implementation(
            builder,
            ClassBlocks {
                implementation: child,
                definition,
            },
            child_depth,
        )?;
    }
    Ok(())
}

/// The first top-level definition of every class `program` also implements,
/// keyed by its case-folded name.
fn implemented_definitions<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    program: Node<'tree>,
) -> Result<BTreeMap<String, Node<'tree>>, ExtractError> {
    let mut implemented = BTreeSet::new();
    let mut definitions = BTreeMap::new();
    for child in named_children(program) {
        builder.context.ensure_active()?;
        let Some(key) = (match child.kind() {
            CLASS_DEFINITION_KIND | CLASS_IMPLEMENTATION_KIND => class_key(builder, child)?,
            _ => None,
        }) else {
            continue;
        };
        if child.kind() == CLASS_IMPLEMENTATION_KIND {
            implemented.insert(key);
        } else {
            definitions.entry(key).or_insert(child);
        }
    }
    definitions.retain(|key, _| implemented.contains(key));
    Ok(definitions)
}

/// The two blocks of one class.
#[derive(Clone, Copy)]
struct ClassBlocks<'tree> {
    /// The `IMPLEMENTATION` block the class symbol spans.
    implementation: Node<'tree>,
    /// The same file's `DEFINITION` block, when it has one.
    definition: Option<Node<'tree>>,
}

/// Declare one implemented class and walk its definition's members and its
/// method implementations inside the class scope.
fn visit_class_implementation(
    builder: &mut ExtractionBuilder<'_, '_>,
    blocks: ClassBlocks<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let ClassBlocks {
        implementation,
        definition,
    } = blocks;
    let Some(name) = field_name(builder, implementation)? else {
        return builder.visit(implementation, depth);
    };
    let declaration = GenericDeclaration {
        kind: SymbolKind::Class,
        name,
        node: implementation,
        modifier_node: definition.unwrap_or(implementation),
    };
    visit_in_scope(builder, declaration, |builder| {
        walk_class_blocks(builder, blocks, depth)
    })
}

/// Emit `declaration`, then run `walk` with it as the innermost owner.
fn visit_in_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: GenericDeclaration<'_>,
    walk: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    let scope = emit_declaration(builder, declaration)?;
    push_scope(builder, scope);
    let walked = walk(builder);
    pop_scope(builder);
    walked
}

/// Walk a definition's members, then the implementation's children. A member
/// method declaration the implementation implements is walked for usages
/// only; its implementation takes the declaration's modifiers and doc comment.
fn walk_class_blocks(
    builder: &mut ExtractionBuilder<'_, '_>,
    blocks: ClassBlocks<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let child_depth = depth.saturating_add(1);
    let implemented = implemented_methods(builder, blocks.implementation)?;
    let mut declarations = BTreeMap::new();
    for section in blocks
        .definition
        .iter()
        .flat_map(|definition| named_children(*definition))
    {
        if !section.kind().ends_with(SECTION_KIND_SUFFIX) {
            builder.visit(section, child_depth)?;
            continue;
        }
        for member in named_children(section) {
            let implemented_name = member_method_name(builder, member)?
                .map(|name| name.to_ascii_lowercase())
                .filter(|name| implemented.contains(name));
            if let Some(name) = implemented_name {
                declarations.entry(name).or_insert(member);
                builder.visit_usage(member, child_depth.saturating_add(1))?;
            } else {
                builder.visit(member, child_depth.saturating_add(1))?;
            }
        }
    }
    for child in named_children(blocks.implementation) {
        let declaration = if child.kind() == METHOD_IMPLEMENTATION_KIND {
            field_name(builder, child)?
                .and_then(|name| declarations.get(&name.to_ascii_lowercase()).copied())
        } else {
            None
        };
        let Some(declaration) = declaration else {
            builder.visit(child, child_depth)?;
            continue;
        };
        let Some(name) = field_name(builder, child)? else {
            continue;
        };
        let method = GenericDeclaration {
            kind: SymbolKind::Method,
            name,
            node: child,
            modifier_node: declaration,
        };
        visit_in_scope(builder, method, |builder| {
            builder.visit_named_children(child, child_depth)
        })?;
    }
    Ok(())
}

/// Case-folded names of the methods an implementation block implements.
fn implemented_methods(
    builder: &mut ExtractionBuilder<'_, '_>,
    implementation: Node<'_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let mut implemented = BTreeSet::new();
    for child in named_children(implementation) {
        builder.context.ensure_active()?;
        if child.kind() == METHOD_IMPLEMENTATION_KIND
            && let Some(name) = field_name(builder, child)?
        {
            implemented.insert(name.to_ascii_lowercase());
        }
    }
    Ok(implemented)
}

/// The case-folded class name of a definition or implementation block; ABAP
/// identifiers are case-insensitive.
fn class_key(
    builder: &mut ExtractionBuilder<'_, '_>,
    class: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    Ok(field_name(builder, class)?.map(|name| name.to_ascii_lowercase()))
}

/// The declaration kind and name of an ABAP node, or `None` when the node
/// declares nothing at its position. Member method declarations declare only
/// inside a declared class or interface; nodes that are not class, interface
/// or method structure follow the generic rules.
pub(super) fn declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<(SymbolKind, String)>, ExtractError> {
    let kind = match node.kind() {
        CLASS_DEFINITION_KIND | CLASS_IMPLEMENTATION_KIND => SymbolKind::Class,
        INTERFACE_KIND => SymbolKind::Interface,
        METHOD_IMPLEMENTATION_KIND => SymbolKind::Method,
        kind if is_member_method_declaration(kind) => {
            let owner = builder.native_owner_kinds.last().copied();
            if !matches!(owner, Some(SymbolKind::Class | SymbolKind::Interface)) {
                return Ok(None);
            }
            return Ok(member_method_name(builder, node)?.map(|name| (SymbolKind::Method, name)));
        }
        _ => return super::declaration(builder, node),
    };
    Ok(field_name(builder, node)?.map(|name| (kind, name)))
}

/// A method implementation's statements, which the grammar attaches as a
/// child rather than a `body` field.
pub(super) fn body(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
        .or_else(|| named_children(node).find(|child| child.kind() == METHOD_BODY_KIND))
}

/// Whether `kind` is a member method declaration of a definition or interface.
fn is_member_method_declaration(kind: &str) -> bool {
    NAMED_METHOD_DECLARATIONS.contains(&kind)
        || KEYWORD_METHOD_DECLARATIONS
            .iter()
            .any(|(declaration, _)| *declaration == kind)
}

/// The name a member method declaration declares, or `None` for any other node.
fn member_method_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if let Some((_, keyword)) = KEYWORD_METHOD_DECLARATIONS
        .iter()
        .find(|(declaration, _)| *declaration == node.kind())
    {
        return Ok(Some((*keyword).to_owned()));
    }
    if NAMED_METHOD_DECLARATIONS.contains(&node.kind()) {
        return field_name(builder, node);
    }
    Ok(None)
}

/// The normalized text of the node's `name` field.
fn field_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(None);
    };
    Ok(normalize_name(&builder.context.owned_text(name)?))
}
