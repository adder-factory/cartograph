//! Intra-procedural definition/use facts for JavaScript-family callables.
//!
//! For every simple-identifier local declared with `const`/`let`/`var`
//! directly in a callable's scope (nested blocks included, nested function
//! scopes and class static blocks excluded), each later occurrence of the
//! same name in that scope is
//! one [`ReferenceKind::DefUse`] site owned by the callable and named by the
//! local. Unused locals, parameters, destructured bindings, member
//! properties, and uses inside nested functions produce nothing — the v1
//! `def_use` contract (`src/extraction/def-use.ts`).
//!
//! Occurrences are syntactic, as in v1: an assignment target (`x = 2`) is a
//! site. Four refinements keep every site attributable to the local it
//! names: a `let`/`const` local is visible only inside its block (a `var` in
//! the whole callable), so a later read outside the block is not its use; a
//! later redeclaration's own name is a definition, not a use; an object
//! shorthand (`{ x }`) reads `x` and is a use; and a name that any other
//! binding form in the scope may shadow (a `catch` parameter, a
//! `for (const x of ..)` variable, a destructured binding, a nested class
//! declaration or named class expression, a nested function declaration)
//! produces no sites at all, because syntax alone
//! cannot tell which binding each occurrence reads. The resolver binds each
//! site to the callable's unique local of that name, so the edge targets the
//! local's declaration rather than v1's self-loop.
//!
//! One bounded pre-order pass collects declarations and identifier
//! occurrences, so the cost is linear in the callable's own scope; nested
//! callables are covered by their own pass.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    AstVisitBudget, ExtractionBuilder, PendingReference, module_system, references,
    syntax::{descendants_including_root, named_children},
};

/// Deepest nesting walked inside one callable scope; a scope the walker
/// already visited never exceeds the walker's own depth limit.
const MAX_DEF_USE_DEPTH: usize = crate::MAXIMUM_AST_DEPTH;

/// Most declarations of one name tracked per callable; a name declared more
/// often is treated as shadowed.
const MAX_DEFINITIONS_PER_NAME: usize = 16;

/// Node kinds whose braces scope a `let`/`const` declaration.
const BLOCK_SCOPE_KINDS: &[&str] = &["statement_block", "for_statement", "switch_body"];

/// Node kinds that open a nested scope with its own `var` declarations: the
/// nested function scopes and class static blocks.
const INNER_SCOPE_KINDS: &[&str] = &[
    "function_declaration",
    "function_expression",
    "arrow_function",
    "method_definition",
    "generator_function_declaration",
    "generator_function",
    "class_static_block",
];

/// One callable body and the symbol that owns its def-use sites.
#[derive(Clone, Copy)]
pub(super) struct DefUseScope<'tree, 'owner> {
    body: Node<'tree>,
    owner: &'owner SymbolId,
}

impl<'tree, 'owner> DefUseScope<'tree, 'owner> {
    pub(super) const fn new(body: Node<'tree>, owner: &'owner SymbolId) -> Self {
        Self { body, owner }
    }
}

/// One simple-identifier declaration and the source range it is visible in.
#[derive(Clone, Copy)]
struct Definition {
    /// Start byte of the declarator; only later occurrences are uses.
    start: usize,
    /// Byte range of the block (`let`/`const`) or callable body (`var`) the
    /// declaration is scoped to.
    visible: (usize, usize),
}

/// Declarations, shadowing bindings, and identifier occurrences of one
/// callable scope.
struct ScopeFacts<'tree, 'text> {
    source: &'text str,
    /// The callable body: the visibility of a `var` declaration.
    body: Node<'tree>,
    /// Declarations of each simple-identifier local.
    definitions: BTreeMap<&'text str, Vec<Definition>>,
    /// Names also bound by a form other than a simple declarator.
    shadowed: BTreeSet<&'text str>,
    /// Identifier occurrences in source order, excluding declaration names.
    occurrences: Vec<Node<'tree>>,
}

/// Emit def-use sites for one JavaScript-family callable body.
pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: DefUseScope<'_, '_>,
) -> Result<(), ExtractError> {
    if !module_system::is_javascript_family(builder.context.snapshot.language())
        || scope.body.kind() != "statement_block"
    {
        return Ok(());
    }
    let snapshot = builder.context.snapshot;
    let mut facts = ScopeFacts {
        source: snapshot.source(),
        body: scope.body,
        definitions: BTreeMap::new(),
        shadowed: BTreeSet::new(),
        occurrences: Vec::new(),
    };
    facts.collect(builder)?;
    for occurrence in std::mem::take(&mut facts.occurrences) {
        if !facts.is_use_of_local(occurrence) {
            continue;
        }
        let name = builder.context.owned_text(occurrence)?;
        references::push_reference(
            builder,
            PendingReference {
                owner: Some(scope.owner.clone()),
                name,
                kind: ReferenceKind::DefUse,
                node: occurrence,
            },
        )?;
    }
    Ok(())
}

impl<'tree> ScopeFacts<'tree, '_> {
    /// One bounded pre-order pass over a callable scope, skipping nested
    /// function scopes and static blocks but recording the names they declare.
    fn collect(&mut self, builder: &mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError> {
        let mut budget = AstVisitBudget::<MAX_DEF_USE_DEPTH>::default();
        let mut pending = vec![(self.body, 0_usize)];
        while let Some((node, depth)) = pending.pop() {
            budget.observe(builder, depth)?;
            self.record(builder, node)?;
            let mut children = Vec::new();
            // A `with` body resolves names against its object first, so no
            // occurrence inside it is attributable to a local.
            let with_body = (node.kind() == "with_statement")
                .then(|| node.child_by_field_name("body"))
                .flatten();
            for child in named_children(node) {
                if with_body.is_some_and(|body| body.id() == child.id())
                    || is_type_subtree(child.kind())
                {
                    continue;
                }
                if INNER_SCOPE_KINDS.contains(&child.kind()) {
                    self.shadow_field(builder, (child, "name"))?;
                } else {
                    children
                        .try_reserve(1)
                        .map_err(|_| ExtractError::OutputLimit)?;
                    children.push((child, depth.saturating_add(1)));
                }
            }
            pending
                .try_reserve(children.len())
                .map_err(|_| ExtractError::OutputLimit)?;
            pending.extend(children.into_iter().rev());
        }
        Ok(())
    }

    /// Record what one node contributes: definitions, shadowing bindings, or
    /// an identifier occurrence.
    fn record(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'tree>,
    ) -> Result<(), ExtractError> {
        match node.kind() {
            "lexical_declaration" | "variable_declaration" => {
                self.record_declaration(builder, node)
            }
            "catch_clause" => self.shadow_field(builder, (node, "parameter")),
            // A `using` resource is a local without a symbol of its own.
            "using_declaration" => self.shadow_declarators(builder, node),
            "for_in_statement" if node.child_by_field_name("kind").is_some() => {
                self.shadow_field(builder, (node, "left"))
            }
            "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "enum_declaration"
            | "internal_module" => self.shadow_field(builder, (node, "name")),
            "identifier" | "shorthand_property_identifier" if !is_declaration_name(node) => {
                self.occurrences
                    .try_reserve(1)
                    .map_err(|_| ExtractError::OutputLimit)?;
                self.occurrences.push(node);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Simple-identifier declarators define locals; destructuring patterns
    /// only shadow the names they bind.
    fn record_declaration(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        declaration: Node<'tree>,
    ) -> Result<(), ExtractError> {
        for declarator in
            named_children(declaration).filter(|child| child.kind() == "variable_declarator")
        {
            let Some(name) = declarator.child_by_field_name("name") else {
                continue;
            };
            if name.kind() != "identifier" {
                self.shadow_tree(builder, name)?;
                continue;
            }
            let Some(text) = self.source.get(name.start_byte()..name.end_byte()) else {
                continue;
            };
            let visible = if declaration.kind() == "variable_declaration" {
                self.body
            } else {
                self.enclosing_block(declaration)
            };
            let definitions = self.definitions.entry(text).or_default();
            if definitions.len() >= MAX_DEFINITIONS_PER_NAME {
                self.shadowed.insert(text);
                continue;
            }
            definitions.push(Definition {
                start: declarator.start_byte(),
                visible: (visible.start_byte(), visible.end_byte()),
            });
        }
        Ok(())
    }

    /// The block a `let`/`const` declaration is scoped to, at most the body.
    fn enclosing_block(&self, declaration: Node<'tree>) -> Node<'tree> {
        let mut current = declaration.parent();
        for _ in 0..MAX_DEF_USE_DEPTH {
            let Some(node) = current else {
                break;
            };
            if node.id() == self.body.id() || BLOCK_SCOPE_KINDS.contains(&node.kind()) {
                return node;
            }
            current = node.parent();
        }
        self.body
    }

    /// Shadow the names a declaration's declarators bind, not the names
    /// their initializers read.
    fn shadow_declarators(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        declaration: Node<'_>,
    ) -> Result<(), ExtractError> {
        for declarator in
            named_children(declaration).filter(|child| child.kind() == "variable_declarator")
        {
            self.shadow_field(builder, (declarator, "name"))?;
        }
        Ok(())
    }

    /// Shadow every name bound by the `(node, field)` child.
    fn shadow_field(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        (node, field): (Node<'_>, &str),
    ) -> Result<(), ExtractError> {
        match node.child_by_field_name(field) {
            Some(binding) => self.shadow_tree(builder, binding),
            None => Ok(()),
        }
    }

    /// Shadow every identifier-shaped name inside a binding. Over-approximating
    /// (default values and computed keys included) only suppresses more sites.
    fn shadow_tree(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        binding: Node<'_>,
    ) -> Result<(), ExtractError> {
        for node in descendants_including_root(binding) {
            builder.context.ensure_active()?;
            if matches!(
                node.kind(),
                "identifier" | "shorthand_property_identifier_pattern" | "type_identifier"
            ) && let Some(text) = self.source.get(node.start_byte()..node.end_byte())
            {
                self.shadowed.insert(text);
            }
        }
        Ok(())
    }

    /// Whether `occurrence` names an unshadowed local declared before it in a
    /// block that encloses it.
    fn is_use_of_local(&self, occurrence: Node<'_>) -> bool {
        let (start, end) = (occurrence.start_byte(), occurrence.end_byte());
        self.source
            .get(start..end)
            .filter(|name| !self.shadowed.contains(name))
            .and_then(|name| self.definitions.get(name))
            .is_some_and(|definitions| {
                definitions.iter().any(|definition| {
                    definition.start < start
                        && definition.visible.0 <= start
                        && end <= definition.visible.1
                })
            })
    }
}

/// Whether a node is TypeScript type syntax, whose identifiers (a function
/// type's or an overload signature's parameters, a `typeof` operand) are
/// never uses of a local.
fn is_type_subtree(kind: &str) -> bool {
    kind.ends_with("_type")
        || matches!(
            kind,
            "type_annotation"
                | "type_arguments"
                | "type_parameters"
                | "function_signature"
                | "method_signature"
                | "abstract_method_signature"
                | "call_signature"
                | "construct_signature"
                | "type_alias_declaration"
                | "interface_declaration"
                | "asserts_annotation"
                | "type_predicate_annotation"
                | "opting_type_annotation"
                | "omitting_type_annotation"
                | "adding_type_annotation"
        )
}

/// Whether an identifier is the bound name of a `variable_declarator`.
fn is_declaration_name(identifier: Node<'_>) -> bool {
    identifier.parent().is_some_and(|parent| {
        parent.kind() == "variable_declarator"
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == identifier.id())
    })
}
