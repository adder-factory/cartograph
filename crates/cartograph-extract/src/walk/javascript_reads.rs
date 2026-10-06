//! Value reads that name module-level bindings in JavaScript-family source.
//!
//! Two v1 reference shapes are restored here:
//!
//! - **Constant reads.** A `SCREAMING_SNAKE` identifier read inside a symbol's
//!   scope (`if (n > MAX_RETRY)`, `return IMPORTED_LIMIT`) is a
//!   [`ReferenceKind::References`] owned by the enclosing symbol. Declaration,
//!   binding, callee, member-property, object-key, assignment-target, and type
//!   positions (`typeof MAX_RETRY.value`) are not reads. Same-file and
//!   imported constants resolve through the ordinary lexical and
//!   import-binding resolvers. A read that a nested binding shadows (a local,
//!   a parameter, a callback, `catch`, or loop binding, a `with` body) is
//!   recorded only when it cannot be mistaken for a module binding of the
//!   same name (see `ReadBinding::resolves_by_name`).
//! - **Binding tables.** A module-level `const`/`let`/`var` whose initializer
//!   is an object or array literal references every module-scope value it
//!   stores — imported or declared at the top level of the file, even when the
//!   name is ambiguous there (`export const ROUTES = { a: handlerA, list:
//!   [handlerB] }`). Only module-level imports count (an import inside a
//!   function declares no module value). Undeclared globals (`window`) are not
//!   recorded, and a value the unique-target value-reference pass already
//!   recorded is not recorded twice.

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference, ImportBindingKind};

use super::{
    AstVisitBudget, ExtractionBuilder, PendingReference,
    javascript_owners::owner_for_node,
    javascript_scopes, module_system, references,
    syntax::{named_children, span_for},
    value_references::MAX_VALUE_REFERENCES,
};

/// Deepest literal nesting walked inside one binding table; the walker's own
/// depth limit already admitted every module-level initializer.
const MAX_BINDING_TABLE_DEPTH: usize = crate::MAXIMUM_AST_DEPTH;
/// Most single-child wrappers (`(x)`, `((x))`) unwrapped when deciding
/// whether an identifier is an assignment target.
const MAX_TARGET_WRAPPERS: usize = 8;
/// Longest member chain (`typeof A.b.c`) climbed to find a type query.
const MAX_TYPE_QUERY_CHAIN: usize = 64;

/// Parent kinds whose identifier children are bindings, never reads.
const BINDING_PARENT_KINDS: &[&str] = &[
    "import_specifier",
    "export_specifier",
    "namespace_import",
    "import_clause",
    "object_pattern",
    "array_pattern",
    "rest_pattern",
    "formal_parameters",
    "catch_clause",
    "labeled_statement",
    "type_query",
    "nested_type_identifier",
    "nested_identifier",
    "jsx_opening_element",
    "jsx_closing_element",
    "jsx_self_closing_element",
];

/// `(parent kind, field)` pairs whose identifier is written or declared.
const NON_READ_FIELDS: &[(&str, &str)] = &[
    ("variable_declarator", "name"),
    ("assignment_expression", "left"),
    ("assignment_pattern", "left"),
    ("object_assignment_pattern", "left"),
    ("pair_pattern", "value"),
    ("required_parameter", "pattern"),
    ("optional_parameter", "pattern"),
    ("for_in_statement", "left"),
    ("arrow_function", "parameter"),
    ("call_expression", "function"),
    ("new_expression", "constructor"),
    ("function_expression", "name"),
    ("generator_function", "name"),
    ("class", "name"),
];

/// Node kinds that open a scope nested inside the module.
const NESTED_SCOPE_KINDS: &[&str] = &[
    "statement_block",
    "class_body",
    "class_static_block",
    "switch_body",
    "formal_parameters",
    "arrow_function",
    "function_expression",
    "function_declaration",
    "generator_function",
    "generator_function_declaration",
    "method_definition",
    "for_statement",
    "for_in_statement",
    "catch_clause",
];

/// Global values that never name a project declaration.
const RESERVED_VALUES: &[&str] = &["undefined", "NaN", "Infinity", "globalThis"];

/// Emit a constant-read reference for `node` when it is a `SCREAMING_SNAKE`
/// identifier read inside a symbol scope, including an object shorthand
/// (`{ MAX_RETRY }` reads `MAX_RETRY`).
pub(super) fn capture_constant_read(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    // The name test is a cheap text check; the position test climbs the
    // syntax tree, so it only runs for constant-shaped names.
    if !matches!(node.kind(), "identifier" | "shorthand_property_identifier")
        || !is_screaming_constant(builder.context.text(node))
        || !is_read_position(node)
    {
        return Ok(());
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return Ok(());
    };
    if !javascript_scopes::read_binding(builder, node)?.resolves_by_name() {
        return Ok(());
    }
    let name = builder.context.owned_text(node)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner),
            name,
            kind: ReferenceKind::References,
            node,
        },
    )
}

/// v1 `SCREAMING_SNAKE_RE`: `^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+$` or
/// `^[A-Z]{2,}\d+[A-Z0-9_]*$`. Single all-caps words (`URL`, `OK`) are
/// commonly type names and are deliberately excluded.
fn is_screaming_constant(name: &str) -> bool {
    is_underscored_constant(name) || is_digit_suffixed_constant(name)
}

/// `^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+$`.
fn is_underscored_constant(name: &str) -> bool {
    let mut segments = name.split('_');
    let Some(first) = segments.next() else {
        return false;
    };
    let first_valid = first
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase())
        && first.bytes().all(is_constant_byte);
    let mut later = 0_usize;
    for segment in segments {
        if segment.is_empty() || !segment.bytes().all(is_constant_byte) {
            return false;
        }
        later = later.saturating_add(1);
    }
    first_valid && later > 0
}

/// `^[A-Z]{2,}\d+[A-Z0-9_]*$`.
fn is_digit_suffixed_constant(name: &str) -> bool {
    let bytes = name.as_bytes();
    let letters = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_uppercase())
        .count();
    let digits = bytes[letters..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    letters >= 2
        && digits > 0
        && bytes[letters + digits..]
            .iter()
            .all(|byte| is_constant_byte(*byte) || *byte == b'_')
}

/// An uppercase ASCII letter or digit.
const fn is_constant_byte(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit()
}

/// Whether an identifier is read rather than declared, bound, called,
/// constructed, or assigned. Parentheses around a target (`(X) = 1`) do not
/// make it a read.
fn is_read_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if BINDING_PARENT_KINDS.contains(&parent.kind()) || is_type_query_operand(node) {
        return false;
    }
    let mut position = node;
    let mut holder = parent;
    for _ in 0..MAX_TARGET_WRAPPERS {
        if holder.kind() != "parenthesized_expression" {
            break;
        }
        let Some(outer) = holder.parent() else {
            break;
        };
        position = holder;
        holder = outer;
    }
    if holder.kind() == "parenthesized_expression" {
        // Unbounded wrapping cannot be classified; never claim it as a read.
        return false;
    }
    !NON_READ_FIELDS.iter().any(|(kind, field)| {
        holder.kind() == *kind
            && holder
                .child_by_field_name(field)
                .is_some_and(|child| child.id() == position.id())
    })
}

/// Whether an identifier roots the operand of a type query (`typeof A`,
/// `typeof A.b.c`, `typeof A[0]`, `typeof f<T>`), which names a value's type
/// rather than reading it.
fn is_type_query_operand(node: Node<'_>) -> bool {
    let mut current = node;
    for _ in 0..MAX_TYPE_QUERY_CHAIN {
        let Some(parent) = current.parent() else {
            return false;
        };
        let operand = match parent.kind() {
            "type_query" => return true,
            "member_expression" | "subscript_expression" => parent.child_by_field_name("object"),
            "instantiation_expression" => parent.child_by_field_name("function"),
            _ => return false,
        };
        if operand.is_none_or(|operand| operand.id() != current.id()) {
            return false;
        }
        current = parent;
    }
    false
}

/// Emit references from module-level binding tables to the imported values
/// they store.
pub(super) fn enrich_binding_tables(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if !module_system::is_javascript_family(builder.context.snapshot.language()) {
        return Ok(());
    }
    let mut module_values = imported_local_names(builder, root)?;
    module_values.extend(top_level_declared_names(builder)?);
    if module_values.is_empty() {
        return Ok(());
    }
    let existing = existing_reference_spans(builder);
    let mut scan = BindingTableScan {
        table: None,
        module_values: &module_values,
        existing: &existing,
        budget: AstVisitBudget::default(),
        references: 0,
    };
    for declarator in module_declarators(root) {
        let Some(value) = declarator
            .child_by_field_name("value")
            .filter(|value| matches!(value.kind(), "object" | "array"))
        else {
            continue;
        };
        scan.table = declarator.child_by_field_name("name");
        scan.visit(builder, value)?;
    }
    Ok(())
}

/// The `variable_declarator`s of top-level `const`/`let`/`var` declarations,
/// including exported ones.
pub(super) fn module_declarators(root: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    named_children(root)
        .filter_map(|statement| {
            if statement.kind() == "export_statement" {
                statement.child_by_field_name("declaration")
            } else {
                Some(statement)
            }
        })
        .filter(|declaration| {
            matches!(
                declaration.kind(),
                "lexical_declaration" | "variable_declaration"
            )
        })
        .flat_map(|declaration| {
            named_children(declaration).filter(|child| child.kind() == "variable_declarator")
        })
}

/// Local names introduced by this file's module-level ES, `CommonJS`, and
/// dynamic imports. A binding inside a function, block, or class body (a
/// function-local `const { x } = require('./m')`) is not a module value.
fn imported_local_names(
    builder: &ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let mut names = BTreeSet::new();
    for binding in &builder.facts.import_bindings {
        if !matches!(
            binding.kind,
            ImportBindingKind::Default | ImportBindingKind::Named | ImportBindingKind::Namespace
        ) || names.contains(&binding.local_name)
        {
            continue;
        }
        let start =
            usize::try_from(binding.span.start_byte()).map_err(|_| ExtractError::InvalidSpan)?;
        let end =
            usize::try_from(binding.span.end_byte()).map_err(|_| ExtractError::InvalidSpan)?;
        if !root
            .descendant_for_byte_range(start, end)
            .is_some_and(|node| {
                is_module_declaration(BindingSite {
                    node,
                    local_name: &binding.local_name,
                    source: builder.context.source(),
                })
            })
        {
            continue;
        }
        builder
            .context
            .budget
            .ensure_string_length(binding.local_name.len())?;
        names.insert(binding.local_name.clone());
    }
    Ok(names)
}

/// One import binding record and the syntax node it was recorded at.
#[derive(Clone, Copy)]
struct BindingSite<'tree, 'text> {
    node: Node<'tree>,
    local_name: &'text str,
    source: &'text str,
}

/// Whether a binding introduces a module-level local: it sits in an `import`
/// clause or in the bound name of a top-level `const`/`let`/`var`, or it is
/// the member a top-level declarator's initializer selects from its module
/// and the declarator binds the record's own local name
/// (`const member = require('./m').member`), outside every function, block,
/// class body, and loop or `catch` scope. A member selected anywhere else
/// (`{ x: (await import('./m')).window }`, `const picked = (..).window`)
/// declares no local of its name.
fn is_module_declaration(site: BindingSite<'_, '_>) -> bool {
    let node = site.node;
    let mut declared = false;
    let mut previous = node;
    let mut current = Some(node);
    for _ in 0..crate::MAXIMUM_AST_DEPTH {
        let Some(ancestor) = current else {
            return declared;
        };
        if NESTED_SCOPE_KINDS.contains(&ancestor.kind()) {
            return false;
        }
        declared |= match ancestor.kind() {
            "import_clause" => true,
            "variable_declarator" => {
                field_is(ancestor, "name", previous)
                    || (selects_module_member(ancestor, node)
                        && ancestor.child_by_field_name("name").is_some_and(|name| {
                            site.source.get(name.start_byte()..name.end_byte())
                                == Some(site.local_name)
                        }))
            }
            _ => false,
        };
        previous = ancestor;
        current = ancestor.parent();
    }
    false
}

/// Whether `child` is the `field` child of `node`.
fn field_is(node: Node<'_>, field: &str, child: Node<'_>) -> bool {
    node.child_by_field_name(field)
        .is_some_and(|value| value.id() == child.id())
}

/// Whether `member` is the property a declarator's whole initializer
/// selects (`require('./m').member`, `(await import('./m')).member`).
fn selects_module_member(declarator: Node<'_>, member: Node<'_>) -> bool {
    let mut value = declarator.child_by_field_name("value");
    for _ in 0..MAX_TARGET_WRAPPERS {
        let Some(current) = value else {
            return false;
        };
        match current.kind() {
            "member_expression" => return field_is(current, "property", member),
            "await_expression" | "parenthesized_expression" | "non_null_expression" => {
                value = current.named_child(0);
            }
            _ => return false,
        }
    }
    false
}

/// Names declared at the top level of this file (not contained by another
/// symbol), excluding the file, import, and export-alias records.
fn top_level_declared_names(
    builder: &ExtractionBuilder<'_, '_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let contained = builder
        .facts
        .containments
        .iter()
        .map(|containment| &containment.child)
        .collect::<BTreeSet<_>>();
    let mut names = BTreeSet::new();
    for symbol in &builder.facts.symbols {
        if matches!(
            symbol.kind,
            SymbolKind::File | SymbolKind::Import | SymbolKind::Export
        ) || contained.contains(&symbol.id)
            || names.contains(&symbol.name)
        {
            continue;
        }
        builder
            .context
            .budget
            .ensure_string_length(symbol.name.len())?;
        names.insert(symbol.name.clone());
    }
    Ok(names)
}

/// Spans already carrying a `references` fact, so a value is never recorded
/// twice when another pass resolved it first.
pub(super) fn existing_reference_spans(
    builder: &ExtractionBuilder<'_, '_>,
) -> BTreeSet<(u64, u64)> {
    builder
        .facts
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| (reference.span.start_byte(), reference.span.end_byte()))
        .collect()
}

/// Bounded walk of one module-level binding-table initializer.
struct BindingTableScan<'scan, 'tree> {
    /// The declared name of the table being scanned; a table that stores
    /// itself is not a reference to another value.
    table: Option<Node<'tree>>,
    /// Imported locals and top-level declared names of this file.
    module_values: &'scan BTreeSet<String>,
    existing: &'scan BTreeSet<(u64, u64)>,
    budget: AstVisitBudget<MAX_BINDING_TABLE_DEPTH>,
    references: usize,
}

impl BindingTableScan<'_, '_> {
    /// Pre-order walk of one initializer with an explicit stack; its depth
    /// limit is the walker's own, which already admitted this subtree.
    fn visit(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        value: Node<'_>,
    ) -> Result<(), ExtractError> {
        let mut pending = vec![(value, 0_usize)];
        while let Some((node, depth)) = pending.pop() {
            if is_binding_table_boundary(node.kind()) {
                continue;
            }
            self.budget.observe(builder, depth)?;
            if is_binding_table_value(node) {
                self.emit(builder, node)?;
            }
            let children = named_children(node).collect::<Vec<_>>();
            pending
                .try_reserve(children.len())
                .map_err(|_| ExtractError::OutputLimit)?;
            pending.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, depth.saturating_add(1))),
            );
        }
        Ok(())
    }

    fn emit(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'_>,
    ) -> Result<(), ExtractError> {
        let text = builder.context.text(node);
        let span = (
            u64::try_from(node.start_byte()).map_err(|_| ExtractError::InvalidSpan)?,
            u64::try_from(node.end_byte()).map_err(|_| ExtractError::InvalidSpan)?,
        );
        let table_name = self.table.map(|table| builder.context.text(table));
        if !self.module_values.contains(text)
            || table_name == Some(text)
            || RESERVED_VALUES.contains(&text)
            || is_screaming_constant(text)
            || self.existing.contains(&span)
        {
            return Ok(());
        }
        self.references = self
            .references
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.references > MAX_VALUE_REFERENCES {
            return Err(ExtractError::OutputLimit);
        }
        let name = builder.context.owned_text(node)?;
        let owner = owner_for_node(builder, node)?;
        builder.emit_reference(ExtractedReference {
            owner,
            name,
            resolution_name: None,
            kind: ReferenceKind::References,
            span: span_for(node)?,
        })
    }
}

/// Function-like values and class bodies leave the binding-table context: a
/// read inside them belongs to that function, not to the table.
fn is_binding_table_boundary(kind: &str) -> bool {
    kind == "class_body" || javascript_scopes::FUNCTION_SCOPE_KINDS.contains(&kind)
}

/// An identifier stored as an array element, a pair value, or an object
/// shorthand property.
fn is_binding_table_value(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match node.kind() {
        "identifier" => {
            parent.kind() == "array"
                || (parent.kind() == "pair"
                    && parent
                        .child_by_field_name("value")
                        .is_some_and(|value| value.id() == node.id()))
        }
        "shorthand_property_identifier" => parent.kind() == "object",
        _ => false,
    }
}
