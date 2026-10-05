//! Lookup names for qualified targets, rebuilt from their syntax.
//!
//! A qualified target (`models.Repo`, `pkg.Base`, `std::fmt::Debug`, the cgo
//! `C.puts`) is looked up by its path. The path is assembled from the name
//! segments the grammar parsed, joined by the language's separator, so layout
//! whitespace and comments never enter the lookup name, and a path with any
//! other segment (`f("x").g`, `8675309 .real`, `<T as Tr>::A`, `a::B<3>::C`)
//! has no lookup name at all: its text could carry literals.

use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

use crate::{ExtractError, walk::ExtractionBuilder};

/// Most name segments one qualified path may have.
const MAX_PATH_SEGMENTS: usize = 32;
/// Path separator for Rust; the other polyglot languages use `.`.
const RUST_SEPARATOR: &str = "::";
/// Path separator for Python attributes and Go package qualifiers.
const DOTTED_SEPARATOR: &str = ".";

/// The lookup name of a qualified `path`, or `None` unless every segment is
/// a plain name.
pub(super) fn lookup_name(
    builder: &ExtractionBuilder<'_, '_>,
    path: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut segments = Vec::new();
    segments
        .try_reserve(MAX_PATH_SEGMENTS)
        .map_err(|_| ExtractError::OutputLimit)?;
    if !collect_segments(path, &mut segments, 0) || segments.is_empty() {
        return Ok(None);
    }
    let separator = if builder.context.snapshot.language() == SourceLanguage::Rust {
        RUST_SEPARATOR
    } else {
        DOTTED_SEPARATOR
    };
    let texts = segments
        .iter()
        .map(|segment| builder.context.text(*segment).trim());
    let length = texts
        .clone()
        .map(|text| text.len().saturating_add(separator.len()))
        .sum::<usize>();
    builder.context.budget.ensure_string_length(length)?;
    let mut lookup = String::new();
    lookup
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    for (index, text) in texts.enumerate() {
        if index > 0 {
            lookup.push_str(separator);
        }
        lookup.push_str(text);
    }
    Ok(Some(lookup))
}

/// Append the name segments of `path` in order; `false` when a segment is not
/// a plain name or the path nests deeper than [`MAX_PATH_SEGMENTS`].
fn collect_segments<'tree>(
    path: Node<'tree>,
    segments: &mut Vec<Node<'tree>>,
    depth: usize,
) -> bool {
    if depth >= MAX_PATH_SEGMENTS || segments.len() >= MAX_PATH_SEGMENTS {
        return false;
    }
    let nested = depth.saturating_add(1);
    let (qualifier, name) = match path.kind() {
        "identifier" | "type_identifier" | "package_identifier" | "field_identifier" | "crate"
        | "self" | "super" => {
            segments.push(path);
            return true;
        }
        "attribute" => (path.child_by_field_name("object"), "attribute"),
        "qualified_type" => (path.child_by_field_name("package"), "name"),
        "selector_expression" => (path.child_by_field_name("operand"), "field"),
        "scoped_type_identifier" | "scoped_identifier" => {
            (path.child_by_field_name("path"), "name")
        }
        _ => return false,
    };
    qualifier.is_none_or(|qualifier| collect_segments(qualifier, segments, nested))
        && path
            .child_by_field_name(name)
            .is_some_and(|name| collect_segments(name, segments, nested))
}
