//! Facts a template expression cannot contribute.
//!
//! A template expression declares nothing, so names it binds itself (an inline
//! handler's parameters and locals, a callback's named function or class) are
//! not graph symbols: an invocation whose callee is rooted at such a name
//! (`handler()`, `item.render()`, `new Local()`) can never reach a script or
//! project declaration of the same name. Names are resolved lexically: a
//! binding hides the script name only inside its own block, function, or
//! clause. A template also binds no module names, so a dynamic import keeps its
//! module reference but not the module bindings (and their imported-name
//! references) that a declaring scope would get.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SourceSpan};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference};

use super::{
    super::{
        AstVisitBudget, ExtractionBuilder, MAX_AST_DEPTH,
        javascript_scopes::FUNCTION_SCOPE_KINDS,
        syntax::{named_children, unwrap_parentheses},
    },
    component_conventions::callee_root_identifier,
};

/// Where one template expression's facts start in the builder.
#[derive(Clone, Copy)]
pub(super) struct ExpressionStart {
    /// Index of the expression's first reference.
    pub(super) reference: usize,
    /// Index of the expression's first module binding.
    pub(super) binding: usize,
}

/// Drop the module bindings, paired imported-name references, and local-name
/// invocations one template expression emitted from `start` on.
pub(super) fn drop_template_local_facts(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    start: ExpressionStart,
) -> Result<(), ExtractError> {
    let binding_spans = builder
        .facts
        .import_bindings
        .split_off(start.binding)
        .into_iter()
        .map(|binding| byte_range(binding.span))
        .collect::<BTreeSet<_>>();
    let local = local_invocations(builder, root)?;
    if binding_spans.is_empty() && local.is_empty() {
        return Ok(());
    }
    let walked = builder.facts.references.split_off(start.reference);
    builder
        .facts
        .references
        .extend(walked.into_iter().filter(|reference| {
            !is_imported_name_reference(reference, &binding_spans)
                && !is_local_invocation(reference, &local)
        }));
    Ok(())
}

/// The reference a dropped module binding paired with its imported name.
fn is_imported_name_reference(
    reference: &ExtractedReference,
    bindings: &BTreeSet<(u64, u64)>,
) -> bool {
    reference.kind == ReferenceKind::References
        && reference.owner.is_none()
        && bindings.contains(&byte_range(reference.span))
}

/// An invocation fact anchored where a locally bound callee starts: the
/// callee of a call (`handler`, `item.render`) or a whole construction
/// (`new Local()`). A method-name fact (`render` of `item.render()`) starts at
/// the property and stays, as it does for a script's local receivers.
fn is_local_invocation(reference: &ExtractedReference, local: &BTreeSet<u64>) -> bool {
    matches!(
        reference.kind,
        ReferenceKind::Calls | ReferenceKind::Instantiates
    ) && local.contains(&reference.span.start_byte())
}

/// Host byte range of a span, as an ordered key.
fn byte_range(span: SourceSpan) -> (u64, u64) {
    (span.start_byte(), span.end_byte())
}

/// Deepest parenthesized wrapping unwrapped from a callee, as reference
/// emission does.
const MAX_CALLEE_PARENTHESES: usize = 64;

/// One step of the scope walk.
enum Step<'tree, 'source> {
    /// Visit a node at an AST depth, binding the names its parent scopes to
    /// it (a function's parameters to its parameter list and body).
    Enter(Node<'tree>, usize, Vec<&'source str>),
    /// Leave a scope, forgetting the names it bound.
    Leave(Vec<&'source str>),
}

/// The bounded walk of one template expression and the names in scope.
struct ScopeWalk<'walk, 'source, 'cancel> {
    builder: &'walk mut ExtractionBuilder<'source, 'cancel>,
    visits: AstVisitBudget<MAX_AST_DEPTH>,
    source: &'source str,
    /// How many enclosing scopes bind each name.
    bound: BTreeMap<&'source str, usize>,
}

/// Names a function binds in its parameter list and in its body. Parameter
/// defaults see the parameters but not the body's `var` declarations, and a
/// computed method name sees neither.
#[derive(Default)]
struct FunctionScopes<'tree, 'source> {
    parameters: Option<Node<'tree>>,
    body: Option<Node<'tree>>,
    parameter_names: Vec<&'source str>,
    body_names: Vec<&'source str>,
}

impl<'tree, 'source> FunctionScopes<'tree, 'source> {
    /// Names `child` of the function binds for its whole extent.
    fn names_for(&self, child: Node<'tree>) -> Vec<&'source str> {
        if Some(child) == self.parameters {
            self.parameter_names.clone()
        } else if Some(child) == self.body {
            self.body_names.clone()
        } else {
            Vec::new()
        }
    }
}

/// Start offsets of the invocation facts whose callee is rooted at a name the
/// expression binds in a scope enclosing the invocation.
fn local_invocations(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<BTreeSet<u64>, ExtractError> {
    let source = builder.context.snapshot.source();
    let mut walk = ScopeWalk {
        builder,
        visits: AstVisitBudget::default(),
        source,
        bound: BTreeMap::new(),
    };
    let mut local = BTreeSet::new();
    let mut steps = vec![Step::Enter(root, 0, Vec::new())];
    while let Some(step) = steps.pop() {
        let (node, depth, mut names) = match step {
            Step::Leave(names) => {
                walk.forget(&names);
                continue;
            }
            Step::Enter(node, depth, names) => (node, depth, names),
        };
        walk.visits.observe(walk.builder, depth)?;
        names.extend(walk.scope_names(node, depth)?);
        walk.remember(&names);
        steps.push(Step::Leave(names));
        local.extend(walk.local_invocation(node));
        let scopes = walk.function_scopes(node, depth)?;
        let child_depth = depth.saturating_add(1);
        steps.extend(
            named_children(node)
                .map(|child| Step::Enter(child, child_depth, scopes.names_for(child))),
        );
    }
    Ok(local)
}

impl<'source> ScopeWalk<'_, 'source, '_> {
    /// Enter a scope binding `names`.
    fn remember(&mut self, names: &[&'source str]) {
        for name in names {
            *self.bound.entry(name).or_default() += 1;
        }
    }

    /// Leave a scope that bound `names`.
    fn forget(&mut self, names: &[&'source str]) {
        for name in names {
            if let Some(count) = self.bound.get_mut(name) {
                *count -= 1;
                if *count == 0 {
                    self.bound.remove(name);
                }
            }
        }
    }

    /// Start offset of the invocation fact of `node` when its callee is rooted
    /// at a name bound in scope. Parentheses around the callee are unwrapped,
    /// as reference emission unwraps them.
    fn local_invocation(&self, node: Node<'_>) -> Option<u64> {
        let (callee, construction) = match node.kind() {
            "call_expression" => (node.child_by_field_name("function")?, None),
            "new_expression" => (node.child_by_field_name("constructor")?, Some(node)),
            _ => return None,
        };
        let callee = unwrap_parentheses(callee, MAX_CALLEE_PARENTHESES)?;
        let root = callee_root_identifier(callee)?;
        let name = self.source.get(root.start_byte()..root.end_byte())?;
        let anchor = construction.unwrap_or(callee);
        self.bound
            .contains_key(name)
            .then(|| u64::try_from(anchor.start_byte()).ok())
            .flatten()
    }

    /// Names a scope node binds for its whole extent.
    fn scope_names(
        &mut self,
        node: Node<'_>,
        depth: usize,
    ) -> Result<Vec<&'source str>, ExtractError> {
        let mut names = Vec::new();
        match node.kind() {
            "program" | "statement_block" | "switch_body" => {
                self.block_names(node, depth, &mut names)?;
            }
            "class_static_block" => self.hoisted_var_names(node, depth, &mut names)?,
            "for_statement" => {
                if let Some(initializer) = node.child_by_field_name("initializer") {
                    self.declarator_names(initializer, depth, &mut names)?;
                }
            }
            _ => {
                if let Some(binding) = clause_binding(node) {
                    self.pattern_names(binding, depth, &mut names)?;
                }
            }
        }
        Ok(names)
    }

    /// Declarations of a block's statements (a `switch`'s case statements),
    /// and for the expression's top level its hoisted `var`s too.
    fn block_names(
        &mut self,
        block: Node<'_>,
        depth: usize,
        names: &mut Vec<&'source str>,
    ) -> Result<(), ExtractError> {
        let statements = if block.kind() == "switch_body" {
            named_children(block)
                .flat_map(named_children)
                .collect::<Vec<_>>()
        } else {
            named_children(block).collect()
        };
        for statement in statements {
            self.block_declaration_names(statement, depth, names)?;
        }
        if block.kind() == "program" {
            self.hoisted_var_names(block, depth, names)?;
        }
        Ok(())
    }

    /// Names a direct child of a block binds in that block: `let`/`const`
    /// declarators and function and class declarations.
    fn block_declaration_names(
        &mut self,
        child: Node<'_>,
        depth: usize,
        names: &mut Vec<&'source str>,
    ) -> Result<(), ExtractError> {
        match child.kind() {
            "lexical_declaration" => self.declarator_names(child, depth, names),
            "function_declaration" | "generator_function_declaration" | "class_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    self.pattern_names(name, depth, names)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The parameter names a function binds in its parameter list and body,
    /// and the `var` declarations hoisted to its body; empty for any other
    /// node.
    fn function_scopes<'tree>(
        &mut self,
        function: Node<'tree>,
        depth: usize,
    ) -> Result<FunctionScopes<'tree, 'source>, ExtractError> {
        let mut scopes = FunctionScopes::default();
        if !is_function_like(function.kind()) {
            return Ok(scopes);
        }
        scopes.parameters = function
            .child_by_field_name("parameters")
            .or_else(|| function.child_by_field_name("parameter"));
        scopes.body = function.child_by_field_name("body");
        if let Some(parameters) = scopes.parameters {
            self.pattern_names(parameters, depth, &mut scopes.parameter_names)?;
        }
        scopes.body_names.clone_from(&scopes.parameter_names);
        if let Some(body) = scopes.body {
            self.hoisted_var_names(body, depth, &mut scopes.body_names)?;
        }
        Ok(scopes)
    }

    /// Names of the declarators of a `let`, `const`, or `var` declaration.
    fn declarator_names(
        &mut self,
        declaration: Node<'_>,
        depth: usize,
        names: &mut Vec<&'source str>,
    ) -> Result<(), ExtractError> {
        if !matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            return Ok(());
        }
        for declarator in named_children(declaration) {
            if let Some(name) = declarator.child_by_field_name("name") {
                self.pattern_names(name, depth, names)?;
            }
        }
        Ok(())
    }

    /// `var` declarations (including `for (var … of …)`) inside `scope`,
    /// outside nested functions and class static blocks, which hoist their own.
    fn hoisted_var_names(
        &mut self,
        scope: Node<'_>,
        depth: usize,
        names: &mut Vec<&'source str>,
    ) -> Result<(), ExtractError> {
        let mut pending = named_children(scope)
            .map(|child| (child, depth.saturating_add(1)))
            .collect::<Vec<_>>();
        while let Some((node, node_depth)) = pending.pop() {
            self.visits.observe(self.builder, node_depth)?;
            if node.kind() == "variable_declaration" {
                self.declarator_names(node, node_depth, names)?;
                continue;
            }
            if let Some(left) = self.for_var_binding(node) {
                self.pattern_names(left, node_depth, names)?;
            }
            if !is_function_like(node.kind()) && node.kind() != "class_static_block" {
                pending.extend(
                    named_children(node).map(|child| (child, node_depth.saturating_add(1))),
                );
            }
        }
        Ok(())
    }

    /// The binding of a `for (var … in/of …)` statement.
    fn for_var_binding<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let kind = node.child_by_field_name("kind")?;
        (node.kind() == "for_in_statement"
            && self.source.get(kind.start_byte()..kind.end_byte()) == Some("var"))
        .then(|| node.child_by_field_name("left"))
        .flatten()
    }

    /// Identifiers a binding pattern binds; default values are not bindings.
    fn pattern_names(
        &mut self,
        pattern: Node<'_>,
        depth: usize,
        names: &mut Vec<&'source str>,
    ) -> Result<(), ExtractError> {
        let mut pending = vec![(pattern, depth.saturating_add(1))];
        while let Some((node, node_depth)) = pending.pop() {
            self.visits.observe(self.builder, node_depth)?;
            let child_depth = node_depth.saturating_add(1);
            match node.kind() {
                // A TypeScript class declaration names itself with a type identifier.
                "identifier" | "shorthand_property_identifier_pattern" | "type_identifier" => {
                    names.extend(self.source.get(node.start_byte()..node.end_byte()));
                }
                "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
                    pending.extend(named_children(node).map(|child| (child, child_depth)));
                }
                _ => pending.extend(pattern_part(node).map(|part| (part, child_depth))),
            }
        }
        Ok(())
    }
}

/// The binding a clause or named expression introduces for its own extent:
/// a function or class expression's name, a catch parameter, or a declared
/// `for … in/of` variable.
fn clause_binding(node: Node<'_>) -> Option<Node<'_>> {
    let field = match node.kind() {
        "function_expression" | "function" | "generator_function" | "class" => "name",
        "catch_clause" => "parameter",
        "for_in_statement" if node.child_by_field_name("kind").is_some() => "left",
        _ => return None,
    };
    node.child_by_field_name(field)
}

/// Whether a node kind is a function, whose parameters and `var`
/// declarations are scoped to it.
fn is_function_like(kind: &str) -> bool {
    kind == "function" || FUNCTION_SCOPE_KINDS.contains(&kind)
}

/// The binding part of a compound pattern element: a parameter's pattern, a
/// property's value, or the target of a defaulted binding.
fn pattern_part(pattern: Node<'_>) -> Option<Node<'_>> {
    let field = match pattern.kind() {
        "required_parameter" | "optional_parameter" => {
            return pattern
                .child_by_field_name("name")
                .or_else(|| pattern.child_by_field_name("pattern"));
        }
        "pair_pattern" => "value",
        "assignment_pattern" | "object_assignment_pattern" => "left",
        _ => return None,
    };
    pattern.child_by_field_name(field)
}
