//! Pascal name, designator, and type-reference spelling helpers.

use tree_sitter::Node;

use crate::walk::syntax::{descendants_including_root, named_children};

/// Bound on dotted components and on the recursion that collects them.
const MAXIMUM_NAME_PARTS: usize = 16;
/// Longest identifier component retained from source.
const MAXIMUM_IDENTIFIER_BYTES: usize = 255;
/// Size of an overload-identity digest.
pub(super) const IDENTITY_BYTES: usize = 32;
/// Domain separation for overload-identity digests.
const IDENTITY_CONTEXT: &str = "cartograph.v2.pascal-overload-identity.2026-10-04";

/// Scalar types the v1 contract never emitted type references for.
const BUILTIN_TYPES: &[&str] = &[
    "boolean", "byte", "cardinal", "char", "double", "integer", "longint", "pointer", "real",
    "single", "string", "word",
];

/// Declared-spelling components of a name: `Foo`, `TFoo.Bar` (dotted), or
/// `TList<T>` (generic arguments dropped).
pub(super) fn name_parts<'source>(
    source: &'source str,
    node: Node<'_>,
) -> Option<Vec<&'source str>> {
    let mut collector = PartCollector {
        source,
        parts: Vec::new(),
    };
    collector.collect(node, 0)?;
    (!collector.parts.is_empty()).then_some(collector.parts)
}

/// Components of the base type named by a `typeref` (pointer and generic
/// arguments dropped), or `None` for anonymous or malformed types.
pub(super) fn type_base_parts<'source>(
    source: &'source str,
    typeref: Node<'_>,
) -> Option<Vec<&'source str>> {
    let named = named_children(typeref).find(|child| {
        matches!(
            child.kind(),
            "identifier" | "typerefDot" | "typerefTpl" | "typerefPtr"
        )
    })?;
    name_parts(source, named)
}

/// Whether a type name is a scalar builtin that never becomes a reference.
pub(super) fn is_builtin_type(name: &str) -> bool {
    BUILTIN_TYPES
        .iter()
        .any(|builtin| builtin.eq_ignore_ascii_case(name))
}

/// Source text of an identifier node when it is a plain Pascal identifier.
/// Delphi's `&` escape (`&type`) names the identifier without the escape. An
/// identifier touching a non-ASCII byte is a fragment of a Unicode identifier
/// the pinned grammar cannot lex (`ber` of `TÜber`), never a name of its own.
pub(super) fn identifier_text<'source>(
    source: &'source str,
    node: Node<'_>,
) -> Option<&'source str> {
    let raw = source.get(node.start_byte()..node.end_byte())?;
    let text = raw.strip_prefix('&').unwrap_or(raw);
    let bytes = source.as_bytes();
    let fragment = [node.start_byte().checked_sub(1), Some(node.end_byte())]
        .into_iter()
        .flatten()
        .any(|adjacent| bytes.get(adjacent).is_some_and(|byte| !byte.is_ascii()));
    (!fragment
        && !text.is_empty()
        && text.len() <= MAXIMUM_IDENTIFIER_BYTES
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(text)
}

/// A fixed-size digest of a node's type syntax for overload identity: its
/// tokens in order, each length-delimited so `array of TItem` never equals a
/// type named `arrayofTItem`. Case, whitespace, identifier escapes (`&`),
/// comments, and directives are ignored. Streaming keeps the cost
/// independent of how often a type is repeated.
pub(super) fn syntax_identity(source: &str, node: Node<'_>) -> [u8; IDENTITY_BYTES] {
    let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_CONTEXT);
    let mut skipped_until = node.start_byte();
    for token in descendants_including_root(node) {
        if matches!(token.kind(), "comment" | "pp") {
            skipped_until = skipped_until.max(token.end_byte());
            continue;
        }
        if token.child_count() > 0 || token.start_byte() < skipped_until {
            continue;
        }
        let text = source
            .get(token.start_byte()..token.end_byte())
            .unwrap_or_default();
        let lower = text.strip_prefix('&').unwrap_or(text).to_lowercase();
        let length = u64::try_from(lower.len()).unwrap_or(u64::MAX);
        hasher.update(&length.to_le_bytes());
        hasher.update(lower.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Lower-case names of the generic parameters a declared name introduces:
/// `T` and `K` of `TBox<T, K>`, and `T` and `U` of `TBox<T>.Get<U>`.
pub(super) fn generic_parameters(source: &str, name: Node<'_>) -> Vec<String> {
    let mut parameters = Vec::new();
    let mut pending = vec![(name, 0_usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAXIMUM_NAME_PARTS {
            continue;
        }
        let next = depth.saturating_add(1);
        match node.kind() {
            "genericDot" => pending.extend(
                [
                    node.child_by_field_name("rhs"),
                    node.child_by_field_name("lhs"),
                ]
                .into_iter()
                .flatten()
                .map(|child| (child, next)),
            ),
            "genericTpl" => pending.extend(
                node.child_by_field_name("args")
                    .map(|arguments| (arguments, next)),
            ),
            "genericArgs" => pending.extend(
                named_children(node)
                    .filter(|child| child.kind() == "genericArg")
                    .map(|child| (child, next)),
            ),
            "genericArg" => {
                let mut cursor = node.walk();
                parameters.extend(
                    node.children_by_field_name("name", &mut cursor)
                        .filter(|child| child.kind() == "identifier")
                        .filter_map(|child| identifier_text(source, child))
                        .map(str::to_ascii_lowercase),
                );
            }
            _ => {}
        }
    }
    parameters
}

/// Source text of a node with every comment and compiler directive inside it
/// replaced by a space, so copied syntax never carries comment content.
pub(super) fn extras_free_text(source: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    let mut text = String::with_capacity(end.saturating_sub(start));
    let mut cursor = start;
    for extra in
        descendants_including_root(node).filter(|child| matches!(child.kind(), "comment" | "pp"))
    {
        if extra.start_byte() < cursor {
            continue;
        }
        text.push_str(source.get(cursor..extra.start_byte()).unwrap_or_default());
        text.push(' ');
        cursor = extra.end_byte();
    }
    text.push_str(source.get(cursor..end).unwrap_or_default());
    text
}

/// Accumulates dotted name components in source order.
struct PartCollector<'source> {
    source: &'source str,
    parts: Vec<&'source str>,
}

impl PartCollector<'_> {
    /// Append the components of one name node, or fail for other shapes.
    fn collect(&mut self, node: Node<'_>, depth: usize) -> Option<()> {
        if depth > MAXIMUM_NAME_PARTS || self.parts.len() >= MAXIMUM_NAME_PARTS {
            return None;
        }
        let next = depth.saturating_add(1);
        match node.kind() {
            "identifier" => self.parts.push(identifier_text(self.source, node)?),
            "genericDot" | "typerefDot" | "exprDot" => {
                self.collect(node.child_by_field_name("lhs")?, next)?;
                self.collect(node.child_by_field_name("rhs")?, next)?;
            }
            "genericTpl" | "typerefTpl" => {
                self.collect(node.child_by_field_name("entity")?, next)?;
            }
            "typerefPtr" => {
                let target = named_children(node).find(|child| child.kind() != "kHat")?;
                self.collect(target, next)?;
            }
            "moduleName" => self.collect_module_name(node)?,
            _ => return None,
        }
        Some(())
    }

    /// Append the identifiers of a dotted unit name.
    fn collect_module_name(&mut self, node: Node<'_>) -> Option<()> {
        for identifier in named_children(node).filter(|child| child.kind() == "identifier") {
            if self.parts.len() >= MAXIMUM_NAME_PARTS {
                return None;
            }
            self.parts.push(identifier_text(self.source, identifier)?);
        }
        Some(())
    }
}
