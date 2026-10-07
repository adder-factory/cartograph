//! Same-file `macro_rules!` macros whose every argument is an expression.
//!
//! A `macro_rules!` macro is in scope from its definition to the end of the
//! block, module body, or file that contains it, and a later definition of the
//! same name shadows an earlier one there. A definition inside a
//! `#[macro_use] mod` stays in scope after the module, to the end of the scope
//! that contains the module (followed through at most [`MAX_MACRO_USE_DEPTH`]
//! nested modules). The walk records each definition with that scope when it
//! reaches it, so an invocation consults the latest definition before it whose
//! scope still contains it, as Rust's textual scoping resolves it.
//!
//! A definition takes expressions when every rule's matcher binds only
//! expression fragments (`$c:expr`, `$c:expr_2021`, `$c:literal`) separated by
//! punctuation (repetition separators included), as in `($cond:expr) => { .. }`: each argument of
//! `ensure!(n < MAX_ITEMS)` is then an ordinary expression, and a
//! constant-shaped name in it is a read, as in a std assertion macro. A matcher
//! with any literal token (`key = $v:expr`), or with an `ident`, `ty`, `path`,
//! `tt`, or other fragment, may treat a token as a key, a declared name, a
//! type, or text, so such a macro keeps the unknown-syntax reading, as does a
//! definition too large to inspect within [`MAX_DEFINITION_MATCHER_NODES`].
//!
//! The registry only feeds optional facts, so the fallback pass that omits
//! them records nothing, and a definition whose name exceeds
//! [`MAX_MACRO_NAME_BYTES`] is skipped rather than allowed to fail the file.

use std::collections::HashMap;

use tree_sitter::Node;

use crate::{
    ExtractError,
    walk::{
        ExtractionBuilder,
        syntax::{children, named_children},
    },
};

/// Fragment specifiers whose matched tokens are one expression.
const EXPRESSION_FRAGMENTS: [&str; 3] = ["expr", "expr_2021", "literal"];
/// Matcher grouping nodes that only nest other matcher parts.
const MATCHER_GROUPS: [&str; 2] = ["token_tree_pattern", REPETITION_GROUP];
/// The `$( .. ) sep op` matcher group, whose separator is not a tree node.
const REPETITION_GROUP: &str = "token_repetition_pattern";
/// Most matcher nodes inspected across one definition's rules; a larger
/// definition keeps the unknown-syntax reading rather than scanning on.
const MAX_DEFINITION_MATCHER_NODES: usize = 4_096;
/// Inspected matcher nodes between cancellation checks.
const MATCHER_CANCELLATION_INTERVAL: usize = 256;
/// Longest macro name recorded; real names are short identifiers.
const MAX_MACRO_NAME_BYTES: usize = 256;
/// Most nested `#[macro_use]` modules a definition's scope is widened through.
const MAX_MACRO_USE_DEPTH: usize = 16;
/// Most outer attributes inspected before a module for `#[macro_use]`.
const MAX_MODULE_ATTRIBUTES: usize = 64;
/// The attribute that keeps a module's macros in scope after the module.
const MACRO_USE_ATTRIBUTE: &str = "macro_use";

/// One recorded definition: the byte range of its scope and its verdict.
#[derive(Clone, Copy)]
struct ScopedDefinition {
    scope_start: usize,
    scope_end: usize,
    takes_expressions: bool,
}

/// The definitions of each `macro_rules!` name recorded so far, in source order.
#[derive(Default)]
pub(in crate::walk) struct LocalExpressionMacros {
    definitions: HashMap<String, Vec<ScopedDefinition>>,
    #[cfg(test)]
    scope_comparisons: std::cell::Cell<usize>,
}

impl LocalExpressionMacros {
    /// A known standard spelling is not a standard macro while a local
    /// definition with that name is in textual scope.
    pub(in crate::walk) fn is_defined(
        &mut self,
        invocation: (&str, usize),
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<bool, ExtractError> {
        Ok(self.current_definition(invocation, cancelled)?.is_some())
    }

    /// Whether the latest definition of `macro_name` in scope at byte `offset`
    /// takes expressions; `false` when none is in scope.
    pub(super) fn takes_expressions(
        &mut self,
        invocation: (&str, usize),
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<bool, ExtractError> {
        Ok(self
            .current_definition(invocation, cancelled)?
            .is_some_and(|definition| definition.takes_expressions))
    }

    /// Source-order traversal never returns to an expired scope. Retire each
    /// binding once; widened `macro_use` scopes retain their original extent.
    fn current_definition(
        &mut self,
        invocation: (&str, usize),
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Option<ScopedDefinition>, ExtractError> {
        let (macro_name, offset) = invocation;
        let Some(definitions) = self.definitions.get_mut(macro_name) else {
            return Ok(None);
        };
        while let Some(definition) = definitions.last().copied() {
            if cancelled() {
                return Err(ExtractError::Cancelled);
            }
            #[cfg(test)]
            self.scope_comparisons.set(self.scope_comparisons.get() + 1);
            if offset < definition.scope_end {
                return Ok((definition.scope_start <= offset).then_some(definition));
            }
            definitions.pop();
        }
        Ok(None)
    }
}

/// Record a `macro_definition`, its scope, and whether its arguments are expressions.
pub(in crate::walk) fn record_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    definition: Node<'_>,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.records() {
        return Ok(());
    }
    let (Some(name_node), Some(scope)) = (
        definition.child_by_field_name("name"),
        definition_scope(builder, definition),
    ) else {
        return Ok(());
    };
    if name_node.end_byte().saturating_sub(name_node.start_byte()) > MAX_MACRO_NAME_BYTES {
        return Ok(());
    }
    let takes_expressions = definition_takes_expressions(builder, definition)?;
    let name = builder.context.owned_text(name_node)?;
    builder
        .rust_macros
        .current_definition((&name, definition.start_byte()), builder.context.cancelled)?;
    let macros = &mut builder.rust_macros.definitions;
    macros
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    let definitions = macros.entry(name).or_default();
    definitions
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    definitions.push(ScopedDefinition {
        scope_start: scope.start_byte(),
        scope_end: scope.end_byte(),
        takes_expressions,
    });
    Ok(())
}

/// The node whose extent a definition is in scope for: its containing block,
/// module body, or file, widened past each enclosing `#[macro_use]` module.
fn definition_scope<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    definition: Node<'tree>,
) -> Option<Node<'tree>> {
    let mut scope = definition.parent()?;
    for _ in 0..MAX_MACRO_USE_DEPTH {
        let Some(module) = scope
            .parent()
            .filter(|module| scope.kind() == "declaration_list" && module.kind() == "mod_item")
        else {
            break;
        };
        if !has_macro_use(builder, module) {
            break;
        }
        scope = module.parent()?;
    }
    Some(scope)
}

/// Whether a module carries an outer `#[macro_use]` attribute.
pub(in crate::walk) fn has_macro_use(
    builder: &ExtractionBuilder<'_, '_>,
    module: Node<'_>,
) -> bool {
    let mut sibling = module.prev_named_sibling();
    for _ in 0..MAX_MODULE_ATTRIBUTES {
        let Some(candidate) = sibling else {
            return false;
        };
        sibling = candidate.prev_named_sibling();
        if candidate.is_extra() {
            continue;
        }
        if candidate.kind() != "attribute_item" {
            return false;
        }
        let names_macro_use = named_children(candidate)
            .find(|attribute| attribute.kind() == "attribute")
            .and_then(|attribute| named_children(attribute).find(|path| !path.is_extra()))
            .is_some_and(|path| builder.context.text(path).trim() == MACRO_USE_ATTRIBUTE);
        if names_macro_use {
            return true;
        }
    }
    false
}

/// Whether the definition has rules and every rule's matcher takes
/// expressions, within one matcher budget shared by all its rules.
fn definition_takes_expressions(
    builder: &mut ExtractionBuilder<'_, '_>,
    definition: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut scan = MatcherScan {
        builder,
        pending: Vec::new(),
        inspected: 0,
    };
    let mut has_rules = false;
    for rule in named_children(definition).filter(|rule| rule.kind() == "macro_rule") {
        has_rules = true;
        let Some(matcher) = rule.child_by_field_name("left") else {
            return Ok(false);
        };
        if !scan.takes_expressions(matcher)? {
            return Ok(false);
        }
    }
    Ok(has_rules)
}

/// A bounded, cancellable walk over one definition's rule matchers.
struct MatcherScan<'scan, 'source, 'cancel, 'tree> {
    builder: &'scan mut ExtractionBuilder<'source, 'cancel>,
    /// Matcher nodes queued for inspection.
    pending: Vec<Node<'tree>>,
    /// Matcher nodes charged so far across the definition's rules.
    inspected: usize,
}

impl<'tree> MatcherScan<'_, '_, '_, 'tree> {
    /// Whether one rule matcher holds only expression bindings, punctuation,
    /// and groups; `false` once the definition's budget is spent.
    fn takes_expressions(&mut self, matcher: Node<'tree>) -> Result<bool, ExtractError> {
        self.pending.clear();
        if !self.queue_children(matcher)? {
            return Ok(false);
        }
        while let Some(node) = self.pending.pop() {
            if node.is_extra() {
                continue;
            }
            if MATCHER_GROUPS.contains(&node.kind()) {
                if !self.separator_is_punctuation(node) || !self.queue_children(node)? {
                    return Ok(false);
                }
                continue;
            }
            if !self.is_expression_token(node) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a nongroup matcher node is an expression binding or punctuation.
    fn is_expression_token(&self, node: Node<'tree>) -> bool {
        if node.kind() == "token_binding_pattern" {
            node.child_by_field_name("type").is_some_and(|fragment| {
                EXPRESSION_FRAGMENTS.contains(&self.builder.context.text(fragment).trim())
            })
        } else {
            !node.is_named() && is_punctuation(node)
        }
    }

    /// Whether a repetition's separator, if any, is punctuation. The grammar
    /// keeps the separator out of the tree, so `$($v:literal) MAX *` would
    /// otherwise pass as a bare repetition and read the word `MAX` in each
    /// invocation as a constant; the text between the group's closing `)`
    /// and the repetition operator is checked instead. Other groups pass.
    fn separator_is_punctuation(&self, group: Node<'tree>) -> bool {
        if group.kind() != REPETITION_GROUP {
            return true;
        }
        let count = group.child_count();
        let (Some(close), Some(operator)) = (
            count.checked_sub(2).and_then(|index| group.child(index)),
            count.checked_sub(1).and_then(|index| group.child(index)),
        ) else {
            return false;
        };
        if close.kind() != ")" {
            return false;
        }
        self.builder
            .context
            .source()
            .get(close.end_byte()..operator.start_byte())
            .is_some_and(|separator| {
                separator
                    .trim()
                    .bytes()
                    .all(|byte| byte.is_ascii_punctuation())
            })
    }

    /// Queue the direct children of `node`, charging each to the budget;
    /// `false` once the budget is spent.
    fn queue_children(&mut self, node: Node<'tree>) -> Result<bool, ExtractError> {
        for child in children(node) {
            if !self.charge()? {
                return Ok(false);
            }
            self.pending
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            self.pending.push(child);
        }
        Ok(true)
    }

    /// Charge one node, polling cancellation every [`MATCHER_CANCELLATION_INTERVAL`]
    /// nodes; `false` once the definition's budget is spent.
    fn charge(&mut self) -> Result<bool, ExtractError> {
        self.inspected = self.inspected.saturating_add(1);
        if self.inspected.is_multiple_of(MATCHER_CANCELLATION_INTERVAL) {
            self.builder.context.ensure_active()?;
        }
        Ok(self.inspected <= MAX_DEFINITION_MATCHER_NODES)
    }
}

/// Whether an anonymous matcher token is punctuation rather than a keyword or word.
fn is_punctuation(token: Node<'_>) -> bool {
    let kind = token.kind();
    !kind.is_empty() && kind.bytes().all(|byte| byte.is_ascii_punctuation())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expired_bindings(count: usize) -> LocalExpressionMacros {
        let mut macros = LocalExpressionMacros::default();
        let mut definitions = vec![ScopedDefinition {
            scope_start: 0,
            scope_end: usize::MAX,
            takes_expressions: false,
        }];
        definitions.extend((0..count).map(|offset| ScopedDefinition {
            scope_start: offset,
            scope_end: offset + 1,
            takes_expressions: true,
        }));
        macros.definitions.insert("q".to_owned(), definitions);
        macros
    }

    #[test]
    fn expired_macro_bindings_are_inspected_only_once() {
        const COUNT: usize = 10_000;
        let mut macros = expired_bindings(COUNT);
        for _ in 0..COUNT {
            assert!(
                !macros
                    .takes_expressions(("q", COUNT), &mut || false)
                    .unwrap_or_else(|error| panic!("lookup failed: {error}"))
            );
        }
        assert!(
            macros.scope_comparisons.get() <= COUNT * 2,
            "expired definitions must not be rescanned: {}",
            macros.scope_comparisons.get()
        );
    }

    #[test]
    fn retiring_expired_macro_bindings_polls_cancellation() {
        let mut macros = expired_bindings(10_000);
        let mut polls = 0;
        let result = macros.takes_expressions(("q", 10_000), &mut || {
            polls += 1;
            polls == 8
        });
        assert!(matches!(result, Err(ExtractError::Cancelled)));
        assert_eq!(polls, 8);
        assert_eq!(macros.scope_comparisons.get(), 7);
    }
}
