//! Framework conventions applied to walked component regions.
//!
//! Compiler-provided names (Svelte runes, Vue macros) are not calls of project
//! code, `$store` identifiers subscribe to Svelte stores, and a component's
//! static imports keep their module reference and bindings on the specifier
//! text, which is how component module imports resolve to files.

use cartograph_domain::{ReferenceKind, SourceSpan, SymbolId};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference};

use super::{
    super::{
        AstVisitBudget, ExtractionBuilder, MAX_AST_DEPTH,
        syntax::{named_children, span_for},
    },
    EmbeddedImportSite,
};

/// Svelte 5 runes: compiler builtins, never calls of project code.
pub(crate) const SVELTE_RUNES: &[&str] = &[
    "$props",
    "$state",
    "$derived",
    "$effect",
    "$bindable",
    "$inspect",
    "$host",
    "$snippet",
];

/// Vue 3 `<script setup>` compiler macros, never calls of project code.
pub(crate) const VUE_COMPILER_MACROS: &[&str] = &[
    "defineProps",
    "defineEmits",
    "defineExpose",
    "defineOptions",
    "defineModel",
    "defineSlots",
    "withDefaults",
];

/// Deepest member chain followed to find the root identifier of a callee.
const MAX_CALLEE_CHAIN_DEPTH: usize = 64;

/// Compiler names of one framework and the first reference of the walked scope.
#[derive(Clone, Copy)]
pub(super) struct CompilerNames<'names> {
    pub(super) names: &'names [&'names str],
    pub(super) first_reference: usize,
}

/// References polled for cancellation at this interval while filtering.
const FILTER_CANCELLATION_INTERVAL: usize = 256;

/// Byte ranges of every compiler invocation of the walked scope, sorted.
///
/// A callee rooted at a compiler name is an identifier or member chain, so no
/// two callees overlap and one binary search finds the callee covering a byte.
struct CompilerInvocations<'names> {
    /// Compiler-provided names of the framework.
    names: &'names [&'names str],
    /// Whole call or construction expressions.
    expressions: Vec<(u64, u64)>,
    /// Invoked callees (`$state`, `$derived.by`, `defineProps`).
    callees: Vec<(u64, u64)>,
}

impl<'names> CompilerInvocations<'names> {
    const fn new(names: &'names [&'names str]) -> Self {
        Self {
            names,
            expressions: Vec::new(),
            callees: Vec::new(),
        }
    }

    fn collect(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        root: Node<'_>,
    ) -> Result<(), ExtractError> {
        let mut visits = AstVisitBudget::<MAX_AST_DEPTH>::default();
        let mut pending = vec![(root, 0_usize)];
        while let Some((node, depth)) = pending.pop() {
            visits.observe(builder, depth)?;
            let callee = match node.kind() {
                "call_expression" => node.child_by_field_name("function"),
                "new_expression" => node.child_by_field_name("constructor"),
                _ => None,
            };
            if let Some(callee) = callee
                && callee_root_identifier(callee).is_some_and(|identifier| {
                    self.names.contains(&builder.context.text(identifier))
                })
            {
                self.expressions.push(byte_range(node)?);
                self.callees.push(byte_range(callee)?);
            }
            pending.extend(named_children(node).map(|child| (child, depth.saturating_add(1))));
        }
        Ok(())
    }

    fn sort(&mut self) {
        self.expressions.sort_unstable();
        self.callees.sort_unstable();
    }

    /// Whether a reference is a compiler name or the fact of one invocation.
    fn covers(&self, reference: &ExtractedReference) -> bool {
        self.names.contains(&reference.name.as_str()) || self.covers_span(reference.span)
    }

    fn covers_span(&self, span: SourceSpan) -> bool {
        let start = span.start_byte();
        let following = self
            .callees
            .partition_point(|(callee_start, _)| *callee_start <= start);
        self.expressions
            .binary_search(&(start, span.end_byte()))
            .is_ok()
            || following
                .checked_sub(1)
                .and_then(|index| self.callees.get(index))
                .is_some_and(|(_, callee_end)| start < *callee_end)
    }
}

/// Remove references the walked scope emitted for compiler-provided names:
/// every reference named exactly like one (v1 parity), and the invocation facts
/// of a call or construction whose callee is rooted at one (`$derived.by(…)`,
/// `new $state()`). Argument and type-argument references stay ordinary code.
pub(super) fn drop_compiler_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    roots: &[Node<'_>],
    scope: CompilerNames<'_>,
) -> Result<(), ExtractError> {
    let mut invocations = CompilerInvocations::new(scope.names);
    for root in roots {
        invocations.collect(builder, *root)?;
    }
    invocations.sort();
    let walked = builder.facts.references.split_off(scope.first_reference);
    for (index, reference) in walked.into_iter().enumerate() {
        if index.is_multiple_of(FILTER_CANCELLATION_INTERVAL) {
            builder.context.ensure_active()?;
        }
        if !invocations.covers(&reference) {
            builder.facts.references.push(reference);
        }
    }
    Ok(())
}

fn byte_range(node: Node<'_>) -> Result<(u64, u64), ExtractError> {
    Ok((
        u64::try_from(node.start_byte()).map_err(|_| ExtractError::InvalidSpan)?,
        u64::try_from(node.end_byte()).map_err(|_| ExtractError::InvalidSpan)?,
    ))
}

/// The identifier a callee is rooted at: the callee itself, or the object at
/// the root of a member chain (`a` of `a.b.c`); none for other callees.
pub(super) fn callee_root_identifier(callee: Node<'_>) -> Option<Node<'_>> {
    let mut current = callee;
    for _ in 0..MAX_CALLEE_CHAIN_DEPTH {
        match current.kind() {
            "identifier" => return Some(current),
            "member_expression" => current = current.child_by_field_name("object")?,
            _ => return None,
        }
    }
    None
}

/// Emit Svelte store subscriptions: a `$count` read or write subscribes to the
/// store `count`. Runes, `$$` internals, and binding positions are not reads.
pub(super) fn emit_store_subscriptions(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    component: &SymbolId,
) -> Result<(), ExtractError> {
    let mut visits = AstVisitBudget::<MAX_AST_DEPTH>::default();
    let mut pending = vec![(root, 0_usize)];
    let mut subscriptions = Vec::new();
    while let Some((node, depth)) = pending.pop() {
        visits.observe(builder, depth)?;
        if matches!(node.kind(), "identifier" | "shorthand_property_identifier")
            && !is_binding_position(node)
            && store_subscription_name(builder.context.text(node)).is_some()
        {
            subscriptions.push(node);
        }
        pending.extend(named_children(node).map(|child| (child, depth.saturating_add(1))));
    }
    subscriptions.sort_by_key(Node::start_byte);
    for node in subscriptions {
        let name = builder.context.owned_text(node)?;
        let Some(store) = store_subscription_name(&name) else {
            continue;
        };
        let resolution_name = builder.context.copy_text(store)?;
        builder.emit_reference(ExtractedReference {
            owner: Some(component.clone()),
            resolution_name: Some(resolution_name),
            kind: ReferenceKind::References,
            span: span_for(node)?,
            name,
        })?;
    }
    Ok(())
}

/// The store a `$name` identifier subscribes to, excluding `$$` internals and runes.
fn store_subscription_name(identifier: &str) -> Option<&str> {
    let store = identifier.strip_prefix('$')?;
    let first = store.chars().next()?;
    (!SVELTE_RUNES.contains(&identifier) && (first == '_' || first.is_ascii_alphabetic()))
        .then_some(store)
}

/// Whether an identifier declares a binding instead of reading one. A
/// parameter's default value (`n = $count`) is a read in either dialect.
fn is_binding_position(identifier: Node<'_>) -> bool {
    let Some(parent) = identifier.parent() else {
        return false;
    };
    let binding_field = match parent.kind() {
        "import_specifier" | "import_clause" | "namespace_import" | "formal_parameters" => {
            return true;
        }
        "required_parameter" | "optional_parameter" => "pattern",
        "assignment_pattern" => "left",
        "variable_declarator" | "function_declaration" | "class_declaration" => "name",
        _ => return false,
    };
    parent.child_by_field_name(binding_field) == Some(identifier)
}

/// The walked script programs of one dialect and their first facts in the builder.
pub(super) struct ProgramImports<'roots, 'tree> {
    pub(super) roots: &'roots [Node<'tree>],
    pub(super) first_reference: usize,
    pub(super) first_binding: usize,
}

/// One static import statement and the specifier its facts anchor on.
struct ImportAnchor {
    statement: (u64, u64),
    module: String,
    anchor: SourceSpan,
}

/// Anchor each static import's module reference and bindings on its specifier
/// text and return the specifiers.
///
/// A component module import resolves to the imported file when an import
/// binding shares the module reference's span (the component convention the
/// line scanner established). The walker anchors the reference on the statement
/// and bindings on their local names, so the convention is restored here;
/// bindings stay resolvable from their uses, which match by local name.
pub(super) fn anchor_static_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
    programs: &ProgramImports<'_, '_>,
) -> Result<Vec<EmbeddedImportSite>, ExtractError> {
    let mut anchors = Vec::new();
    let mut sites = Vec::new();
    for root in programs.roots {
        for statement in named_children(*root).filter(|node| node.kind() == "import_statement") {
            builder.context.ensure_active()?;
            let Some(specifier) = statement.child_by_field_name("source").and_then(|source| {
                super::super::module_system::screened_import_source(builder, source)?;
                named_children(source).find(|part| part.kind() == "string_fragment")
            }) else {
                continue;
            };
            let module = builder.context.owned_text(specifier)?;
            if super::super::specifier_safety::specifier_may_carry_credential(&module) {
                continue;
            }
            sites.push(EmbeddedImportSite {
                module: builder.context.copy_text(&module)?,
                start: specifier.start_byte(),
                end: specifier.end_byte(),
            });
            anchors.push(ImportAnchor {
                statement: byte_range(statement)?,
                module,
                anchor: span_for(specifier)?,
            });
        }
    }
    anchors.sort_by_key(|anchor| anchor.statement);
    for reference in &mut builder.facts.references[programs.first_reference..] {
        if reference.kind == ReferenceKind::Imports
            && let Some(anchor) = statement_anchor(&anchors, reference.span)
            && anchor.module == reference.name
        {
            reference.span = anchor.anchor;
        }
    }
    for binding in &mut builder.facts.import_bindings[programs.first_binding..] {
        if let Some(anchor) = statement_anchor(&anchors, binding.span)
            && anchor.module == binding.module_specifier
        {
            binding.span = anchor.anchor;
        }
    }
    Ok(sites)
}

/// The import statement, among statements sorted by position, enclosing `span`.
fn statement_anchor(anchors: &[ImportAnchor], span: SourceSpan) -> Option<&ImportAnchor> {
    let following = anchors.partition_point(|anchor| anchor.statement.0 <= span.start_byte());
    let anchor = anchors.get(following.checked_sub(1)?)?;
    (span.end_byte() <= anchor.statement.1).then_some(anchor)
}
