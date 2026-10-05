//! Ruby local-variable scopes tracked in source order.
//!
//! Ruby decides at parse time whether a bare identifier is a local read or a
//! method call: it is a local once an assignment or parameter earlier in the
//! same scope has bound it. Methods, class and module bodies, and the program
//! start fresh scopes; blocks and lambdas see the enclosing locals and add
//! their own, which disappear when the block ends.

use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::ExtractError;

use super::super::{ExtractionBuilder, syntax::named_children};

/// Parameter lists whose direct identifier children bind locals.
const PARAMETER_LIST_KINDS: [&str; 4] = [
    "method_parameters",
    "block_parameters",
    "lambda_parameters",
    "destructured_parameter",
];
/// Parameter forms whose `name` field binds a local.
const NAMED_PARAMETER_KINDS: [&str; 5] = [
    "optional_parameter",
    "keyword_parameter",
    "splat_parameter",
    "hash_splat_parameter",
    "block_parameter",
];
/// Multiple-assignment targets whose identifier children bind locals.
const TARGET_LIST_KINDS: [&str; 4] = [
    "left_assignment_list",
    "destructured_left_assignment",
    "rest_assignment",
    "exception_variable",
];
/// Pattern-matching containers (`case ... in`, `=>`, `in`) whose identifier
/// children bind locals; a pinned `^name` is a `variable_reference_pattern`
/// and binds nothing.
const PATTERN_LIST_KINDS: [&str; 5] = [
    "array_pattern",
    "find_pattern",
    "hash_pattern",
    "alternative_pattern",
    "parenthesized_pattern",
];
/// Maximum nesting followed inside one parameter list or assignment target.
const MAX_BINDING_DEPTH: usize = 16;

/// Stack of local-variable scopes for the Ruby file being walked.
#[derive(Default)]
pub(super) struct RubyLocals {
    frames: Vec<LocalFrame>,
}

struct LocalFrame {
    names: BTreeSet<String>,
    /// Whether enclosing frames are invisible from this one.
    fresh: bool,
}

/// Kind of scope a construct opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScopeKind {
    /// Method, class/module body, or program: enclosing locals are hidden.
    Fresh,
    /// Block or lambda: enclosing locals stay visible.
    Nested,
}

impl RubyLocals {
    fn push(&mut self, kind: ScopeKind) {
        self.frames.push(LocalFrame {
            names: BTreeSet::new(),
            fresh: kind == ScopeKind::Fresh,
        });
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    fn bind(&mut self, name: String) {
        if let Some(frame) = self.frames.last_mut() {
            frame.names.insert(name);
        }
    }

    /// Whether `name` is a local visible from the innermost scope.
    pub(super) fn is_local(&self, name: &str) -> bool {
        for frame in self.frames.iter().rev() {
            if frame.names.contains(name) {
                return true;
            }
            if frame.fresh {
                break;
            }
        }
        false
    }
}

/// A scope about to be entered, with the parameter list it binds on entry.
#[derive(Clone, Copy)]
pub(super) struct LocalScope<'tree> {
    pub(super) kind: ScopeKind,
    pub(super) parameters: Option<Node<'tree>>,
}

/// Run `action` inside a new local scope whose parameters are bound first.
pub(super) fn within_scope<Output>(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: LocalScope<'_>,
    action: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<Output, ExtractError>,
) -> Result<Output, ExtractError> {
    builder.script.ruby.locals.push(scope.kind);
    let result = match scope.parameters {
        Some(parameters) => bind_targets(builder, parameters, 0),
        None => Ok(()),
    }
    .and_then(|()| action(builder));
    builder.script.ruby.locals.pop();
    result
}

/// Bind every local introduced by a parameter list or assignment target.
pub(super) fn bind_targets(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if depth > MAX_BINDING_DEPTH {
        return Ok(());
    }
    let kind = target.kind();
    if kind == "identifier" {
        let name = builder
            .context
            .copy_text(builder.context.text(target).trim())?;
        builder.script.ruby.locals.bind(name);
    } else if NAMED_PARAMETER_KINDS.contains(&kind) {
        if let Some(name) = target.child_by_field_name("name") {
            bind_targets(builder, name, depth.saturating_add(1))?;
        }
    } else if PARAMETER_LIST_KINDS.contains(&kind)
        || TARGET_LIST_KINDS.contains(&kind)
        || PATTERN_LIST_KINDS.contains(&kind)
    {
        for child in named_children(target) {
            builder.context.ensure_active()?;
            bind_targets(builder, child, depth.saturating_add(1))?;
        }
    } else if kind == "as_pattern" || kind == "keyword_pattern" {
        bind_pattern_fields(builder, target, depth)?;
    }
    Ok(())
}

/// `pattern => name` binds `name` and whatever `pattern` binds; a keyword
/// pattern binds its value pattern, or its own key when it has none
/// (`in {name:}` binds `name`).
fn bind_pattern_fields(
    builder: &mut ExtractionBuilder<'_, '_>,
    pattern: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let next = depth.saturating_add(1);
    if let Some(value) = pattern.child_by_field_name("value") {
        bind_targets(builder, value, next)?;
    }
    if let Some(name) = pattern.child_by_field_name("name") {
        bind_targets(builder, name, next)?;
    }
    let key_only =
        pattern.kind() == "keyword_pattern" && pattern.child_by_field_name("value").is_none();
    if let Some(key) = pattern
        .child_by_field_name("key")
        .filter(|key| key_only && key.kind() == "hash_key_symbol")
    {
        let name = builder
            .context
            .copy_text(builder.context.text(key).trim())?;
        builder.script.ruby.locals.bind(name);
    }
    Ok(())
}
