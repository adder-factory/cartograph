//! Astro component extraction.
//!
//! Every `.astro` file is one component named after its file. The frontmatter
//! fence is a TypeScript program and the script text of `{…}` template and
//! attribute expressions is TypeScript; both run through the JavaScript-family
//! walker as embedded regions owned by the component. Markup nested inside an
//! expression stays Astro markup, so the Astro grammar (which knows void
//! elements and HTML comments) keeps scanning it while the expression's script
//! pieces are parsed as one stream. Capitalized template tags are
//! component uses, so they become references from the component rather than
//! declarations. As in v1, client `<script>` and `<style>` elements are not part
//! of the component's server-side module and are not extracted.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{
    ExtractError,
    budget::symbol_budget_bytes,
    custom::{basename_stem, file_component_symbol},
    source_lines::{LineMap, SourceByteRange},
};

use super::{
    AstVisitBudget, ExtractionBuilder, MAX_AST_DEPTH, PendingReference,
    embedded_script::{
        ASTRO_SCRIPT_POLICY, EmbeddedComponent, EmbeddedFacts, EmbeddedLease, EmbeddedRequest,
        ScriptDialect, ScriptRegion, extract_component_scripts,
    },
    references::push_reference,
    syntax::{named_children, starts_uppercase},
};

/// Astro component scripts and expressions are TypeScript.
const ASTRO_DIALECT: ScriptDialect = ScriptDialect::TypeScript;

/// Handle the whole Astro document at its root; every other node is reached
/// from here, so the generic traversal never descends into markup.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    _depth: usize,
) -> Result<bool, ExtractError> {
    if node.parent().is_some() {
        return Ok(false);
    }
    extract_component(builder, node)?;
    Ok(true)
}

fn extract_component(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    let snapshot = builder.context.snapshot;
    if snapshot.source().is_empty() {
        return Ok(());
    }
    let component = emit_file_component(builder)?;
    let document = scan_document(builder, root)?;
    for tag in document.component_tags {
        let tag_name = builder.context.owned_text(tag)?;
        push_reference(
            builder,
            PendingReference {
                owner: Some(component.clone()),
                name: tag_name,
                kind: ReferenceKind::References,
                node: tag,
            },
        )?;
    }
    let facts = extract_component_scripts(
        EmbeddedRequest {
            component: EmbeddedComponent {
                snapshot,
                id: &component,
                policy: ASTRO_SCRIPT_POLICY,
                maximum_ast_depth: builder.maximum_ast_depth,
            },
            regions: &document.regions,
            lease: EmbeddedLease {
                identities: &mut builder.identities,
                budget: &mut builder.context.budget,
            },
        },
        &mut *builder.context.cancelled,
    )?;
    absorb_component_facts(builder, facts);
    Ok(())
}

/// Emit the exported file-level component spanning the whole file, shaped
/// exactly like the Vue and Svelte file components.
fn emit_file_component(builder: &mut ExtractionBuilder<'_, '_>) -> Result<SymbolId, ExtractError> {
    let snapshot = builder.context.snapshot;
    let source = snapshot.source();
    let name = builder
        .context
        .copy_text(basename_stem(snapshot.path().as_str()))?;
    let span = LineMap::new(source)?.span(SourceByteRange::new(0, source.len(), source.len()))?;
    let id = builder.identities.next(SymbolKind::Component, &name)?;
    let symbol = file_component_symbol(id.clone(), &name, span)?;
    builder.context.budget.reserve_fact(
        symbol_budget_bytes(&symbol),
        [
            symbol.name.as_str(),
            symbol.qualified_name.as_str(),
            symbol.body_search_text.as_str(),
        ],
    )?;
    builder.facts.symbols.push(symbol);
    Ok(id)
}

fn absorb_component_facts(builder: &mut ExtractionBuilder<'_, '_>, mut facts: EmbeddedFacts) {
    builder.facts.symbols.append(&mut facts.symbols);
    builder.facts.containments.append(&mut facts.containments);
    builder.facts.references.append(&mut facts.references);
    builder
        .facts
        .import_bindings
        .append(&mut facts.import_bindings);
    builder.embedded.diagnostics.append(&mut facts.diagnostics);
    builder.shortened_canonical_names |= facts.shortened_canonical_names;
}

/// Embedded regions and component-use tags of one Astro document.
#[derive(Default)]
struct AstroDocument<'tree> {
    regions: Vec<ScriptRegion>,
    component_tags: Vec<Node<'tree>>,
}

/// Whether the scan continues into a node's children.
#[derive(PartialEq, Eq)]
enum Descend {
    Children,
    Skip,
}

fn scan_document<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'tree>,
) -> Result<AstroDocument<'tree>, ExtractError> {
    let mut document = AstroDocument::default();
    let mut visits = AstVisitBudget::<MAX_AST_DEPTH>::default();
    let mut pending = vec![(root, 0_usize)];
    while let Some((node, depth)) = pending.pop() {
        visits.observe(builder, depth)?;
        if scan_node(builder, node, &mut document) == Descend::Children {
            pending.extend(named_children(node).map(|child| (child, depth.saturating_add(1))));
        }
    }
    document.regions.sort_by_key(ScriptRegion::start);
    document.component_tags.sort_by_key(Node::start_byte);
    Ok(document)
}

fn scan_node<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
    document: &mut AstroDocument<'tree>,
) -> Descend {
    match node.kind() {
        "frontmatter_js_block" => document.regions.push(ScriptRegion::program(
            node.start_byte(),
            node.end_byte(),
            ASTRO_DIALECT,
        )),
        "attribute_js_expr" => document.regions.push(ScriptRegion::template_expression(
            node.start_byte(),
            node.end_byte(),
            ASTRO_DIALECT,
        )),
        // One expression is parsed as a single stream of its script pieces;
        // markup nested inside it is scanned as markup. An interpolation
        // nested directly in another is a script brace of the outer one.
        "html_interpolation" => {
            if node
                .parent()
                .is_none_or(|parent| parent.kind() != "html_interpolation")
            {
                let pieces = interpolation_pieces(builder, node);
                if !pieces.is_empty() {
                    document
                        .regions
                        .push(ScriptRegion::template_pieces(pieces, ASTRO_DIALECT));
                }
            }
            return Descend::Children;
        }
        "tag_name" => {
            if is_component_use(builder, node) {
                document.component_tags.push(node);
            }
        }
        "permissible_text" | "script_element" | "style_element" => {}
        _ => return Descend::Children,
    }
    Descend::Skip
}

/// The script text between an expression's braces: everything except the
/// markup elements nested in it (directly or through its own script braces).
fn interpolation_pieces(
    builder: &ExtractionBuilder<'_, '_>,
    interpolation: Node<'_>,
) -> Vec<(usize, usize)> {
    let source = builder.context.snapshot.source().as_bytes();
    let start = interpolation.start_byte();
    let end = interpolation.end_byte();
    let mut cursor = if source.get(start) == Some(&b'{') {
        start.saturating_add(1)
    } else {
        start
    };
    let inner_end = if end > cursor && source.get(end.saturating_sub(1)) == Some(&b'}') {
        end.saturating_sub(1)
    } else {
        end
    };
    let mut pieces = Vec::new();
    for markup in nested_markup(interpolation) {
        if cursor < markup.start_byte() {
            pieces.push((cursor, markup.start_byte()));
        }
        cursor = cursor.max(markup.end_byte());
    }
    if cursor < inner_end {
        pieces.push((cursor, inner_end));
    }
    pieces
}

/// Markup nodes inside an expression, in source order, without descending
/// into them.
fn nested_markup(interpolation: Node<'_>) -> Vec<Node<'_>> {
    let mut markup = Vec::new();
    let mut pending = vec![interpolation];
    while let Some(node) = pending.pop() {
        for child in named_children(node) {
            match child.kind() {
                "html_interpolation" => pending.push(child),
                "permissible_text" => {}
                _ => markup.push(child),
            }
        }
    }
    markup.sort_by_key(Node::start_byte);
    markup
}

/// A capitalized opening or self-closing tag name is a component use.
fn is_component_use(builder: &ExtractionBuilder<'_, '_>, tag_name: Node<'_>) -> bool {
    tag_name
        .parent()
        .is_some_and(|parent| matches!(parent.kind(), "start_tag" | "self_closing_tag"))
        && starts_uppercase(builder.context.text(tag_name))
}
