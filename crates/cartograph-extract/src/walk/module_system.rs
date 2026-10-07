mod require_arguments;

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol,
    javascript_bindings::{BindingMatch, scan_bound_names},
    references, require_aliases,
    specifier_safety::specifier_may_carry_credential,
    syntax::{descendants_including_root, has_child_kind, named_children, span_for},
};

/// `import()` takes a specifier and an optional import-attributes object.
const MAX_DYNAMIC_IMPORT_ARGUMENTS: usize = 2;
/// Parentheses, assertions, and awaits followed around one dynamic import.
const MAX_DYNAMIC_IMPORT_WRAPPER_DEPTH: usize = 8;
/// Members of the promise returned by `import()`, before it is awaited.
const DYNAMIC_IMPORT_PROMISE_METHODS: [&str; 3] = ["then", "catch", "finally"];
/// Local name of the binding an inline import type creates. It is not a
/// JavaScript identifier, so no ordinary reference name equals or extends it.
const INLINE_IMPORT_TYPE_LOCAL_NAME: &str = "import()";
/// Deepest member chain followed to the root of a write target.
const MAX_MEMBER_WRITE_DEPTH: usize = 64;
/// Most nodes of one assignment or loop target scanned for `createRequire`
/// alias writes.
const MAX_ALIAS_TARGET_NODES: usize = 4 * crate::MAXIMUM_AST_DEPTH;

struct CommonJsRequire<'tree> {
    call: Node<'tree>,
    selected_member: Option<Node<'tree>>,
    module_specifier: String,
}

#[derive(Clone, Copy)]
struct ImportDestructuringInput<'module> {
    module_specifier: &'module str,
    excluded_members: &'module [&'module str],
}

pub(super) struct ExportAlias<'tree> {
    pub(super) public_name: String,
    pub(super) local_name: String,
    pub(super) span_node: Node<'tree>,
    pub(super) reference_node: Node<'tree>,
    pub(super) source: Option<String>,
}

#[derive(Default)]
pub(super) struct CommonJsShadowing {
    require: bool,
    module: bool,
    exports: bool,
    /// Top-level bindings created by the Node.js `createRequire(..)`, which
    /// load modules exactly like `require`.
    require_aliases: BTreeSet<String>,
    /// Factory and module locals every alias was proven through.
    require_fences: BTreeSet<String>,
    /// How often each alias, and each factory or module local the aliases
    /// were proven through, is bound or written anywhere in the file.
    alias_bindings: BTreeMap<String, usize>,
    /// The aliases that still prove a `require` call once every binding and
    /// write of the file has been counted.
    proven_aliases: BTreeSet<String>,
}

impl CommonJsShadowing {
    fn record(&mut self, name: &str) {
        let name = name.trim();
        match name {
            "require" => self.require = true,
            "module" => self.module = true,
            "exports" => self.exports = true,
            _ => {}
        }
        if let Some(bindings) = self.alias_bindings.get_mut(name) {
            *bindings = bindings.saturating_add(1);
        }
    }

    /// Whether any binding in the file may shadow the global `require`.
    pub(super) const fn shadows_require(&self) -> bool {
        self.require
    }

    /// Whether `name` is a proven `createRequire` alias.
    fn is_require_alias(&self, name: &str) -> bool {
        self.proven_aliases.contains(name)
    }

    /// Settle the aliases once every binding and write is counted: an alias
    /// proves `require` only when its name and every factory and module local
    /// are each bound exactly once and never written. Any second binding (a
    /// parameter, a local, a loop variable, a class expression name) or write
    /// may replace one of them, so the file-wide model conservatively stops
    /// treating it as `require`.
    fn settle_aliases(&mut self) {
        let fences_hold = self
            .require_fences
            .iter()
            .all(|fence| self.bound_once(fence));
        self.proven_aliases = if fences_hold {
            self.require_aliases
                .iter()
                .filter(|alias| self.bound_once(alias))
                .cloned()
                .collect()
        } else {
            BTreeSet::new()
        };
    }

    /// Whether a tracked alias or fence name is bound exactly once.
    fn bound_once(&self, name: &str) -> bool {
        self.alias_bindings.get(name) == Some(&1)
    }
}

pub(super) fn collect_explicit_exports(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if !is_javascript_family(builder.context.snapshot.language()) {
        return Ok(());
    }
    collect_commonjs_shadowing(builder, root)?;
    let collected = require_aliases::collect(builder, root)?;
    if !collected.aliases.is_empty() {
        for name in collected.aliases.iter().chain(&collected.fences) {
            builder
                .commonjs_shadowing
                .alias_bindings
                .insert(name.clone(), 0);
        }
        builder.commonjs_shadowing.require_aliases = collected.aliases;
        builder.commonjs_shadowing.require_fences = collected.fences;
        // A second binding pass counts how often each alias and fence name is
        // bound or written; it runs only for files that create an alias.
        collect_commonjs_shadowing(builder, root)?;
        builder.commonjs_shadowing.settle_aliases();
    }
    for node in descendants_including_root(root) {
        builder.context.ensure_active()?;
        match node.kind() {
            "export_statement" if node.child_by_field_name("source").is_none() => {
                collect_local_export_statement(builder, node)?;
            }
            "assignment_expression" => collect_commonjs_export_assignment(builder, node)?,
            _ => {}
        }
    }
    Ok(())
}

fn collect_commonjs_shadowing(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    for node in descendants_including_root(root) {
        builder.context.ensure_active()?;
        match node.kind() {
            "variable_declarator" => record_binding_field(builder, node, "name")?,
            "class_declaration" | "abstract_class_declaration" | "class" => {
                record_class_name(builder, node);
            }
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "method_definition"
            | "arrow_function" => record_callable_bindings(builder, node)?,
            "import_statement" => record_binding_tree(builder, node)?,
            "catch_clause" => record_binding_field(builder, node, "parameter")?,
            "for_in_statement"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "update_expression" => {
                record_alias_rebinding(builder, node);
            }
            "unary_expression" if is_delete(builder, node) => record_alias_rebinding(builder, node),
            _ => {}
        }
    }
    Ok(())
}

/// A `createRequire` alias, or the factory or module local it was proven
/// through, rebound by a loop variable (`for (const load of ..)`) or written
/// (`load = other`, `load++`, `delete Module.createRequire`,
/// `({ createRequire } = other)`) no longer proves a
/// `require` call; a member write (`load.cache = ..`) leaves it intact. Only
/// alias counts change: the `require`/`module`/`exports` model keeps its
/// established binding rules. A target too large to scan invalidates every
/// alias.
fn record_alias_rebinding(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) {
    if builder.commonjs_shadowing.alias_bindings.is_empty() {
        return;
    }
    let Some(target) = node
        .child_by_field_name("left")
        .or_else(|| node.child_by_field_name("argument"))
    else {
        return;
    };
    let source = builder.context.snapshot.source();
    let shadowing = &mut builder.commonjs_shadowing;
    // A write through a module-object fence (`Module.createRequire = fake`)
    // replaces the factory every alias was proven through.
    if let Some(root) =
        member_write_root(target).and_then(|root| source.get(root.start_byte()..root.end_byte()))
        && shadowing.require_fences.contains(root)
        && let Some(bindings) = shadowing.alias_bindings.get_mut(root)
    {
        *bindings = bindings.saturating_add(1);
    }
    let aliases = &mut shadowing.alias_bindings;
    let mut budget = MAX_ALIAS_TARGET_NODES;
    let outcome = scan_bound_names(target, &mut budget, |bound| {
        let name = source
            .get(bound.start_byte()..bound.end_byte())
            .unwrap_or_default();
        if let Some(bindings) = aliases.get_mut(name) {
            *bindings = bindings.saturating_add(1);
        }
        false
    });
    if outcome == BindingMatch::Exhausted {
        for bindings in aliases.values_mut() {
            *bindings = bindings.saturating_add(1);
        }
    }
}

/// The identifier at the root of a member or subscript write target
/// (`Module` in `Module.createRequire = ..`).
fn member_write_root(target: Node<'_>) -> Option<Node<'_>> {
    let mut current = target;
    for _ in 0..MAX_MEMBER_WRITE_DEPTH {
        match current.kind() {
            "member_expression" | "subscript_expression" => {
                current = current.child_by_field_name("object")?;
            }
            "identifier" if current.id() != target.id() => return Some(current),
            _ => return None,
        }
    }
    None
}

/// A class name binds like any other name (a TypeScript class name is a
/// `type_identifier`, which binding trees do not record).
fn record_class_name(builder: &mut ExtractionBuilder<'_, '_>, class: Node<'_>) {
    if let Some(name) = class.child_by_field_name("name") {
        builder
            .commonjs_shadowing
            .record(builder.context.text(name));
    }
}

/// Whether a unary expression deletes its operand (`delete Module.createRequire`).
fn is_delete(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    node.child_by_field_name("operator")
        .is_some_and(|operator| builder.context.text(operator) == "delete")
}

fn record_callable_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    record_binding_field(builder, node, "name")?;
    if let Some(parameters) = node
        .child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("parameter"))
    {
        record_binding_tree(builder, parameters)?;
    }
    Ok(())
}

fn record_binding_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    field: &str,
) -> Result<(), ExtractError> {
    let Some(binding) = node.child_by_field_name(field) else {
        return Ok(());
    };
    record_binding_tree(builder, binding)
}

fn record_binding_tree(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    for node in descendants_including_root(root) {
        builder.context.ensure_active()?;
        if matches!(
            node.kind(),
            "identifier" | "property_identifier" | "shorthand_property_identifier_pattern"
        ) {
            builder
                .commonjs_shadowing
                .record(builder.context.text(node));
        }
    }
    Ok(())
}

pub(super) fn capture_commonjs_require(
    builder: &mut ExtractionBuilder<'_, '_>,
    name_node: Node<'_>,
    value: Option<Node<'_>>,
) -> Result<(), ExtractError> {
    if !is_javascript_family(builder.context.snapshot.language()) {
        return Ok(());
    }
    let Some(require) = parse_commonjs_require(builder, value)? else {
        return Ok(());
    };
    // The module is loaded even when the binding shape (an array pattern, or a
    // destructured member selection) has no modeled import binding.
    capture_commonjs_binding(builder, name_node, &require)?;
    emit_commonjs_module_reference(builder, require)
}

pub(super) fn capture_dynamic_import_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    name_node: Node<'_>,
    value: Option<Node<'_>>,
) -> Result<(), ExtractError> {
    if !is_javascript_family(builder.context.snapshot.language()) {
        return Ok(());
    }
    let Some((call, selected_member)) = dynamic_import_binding_shape(builder, value) else {
        return Ok(());
    };
    let Some(source) = dynamic_import_source(builder, call) else {
        return Ok(());
    };
    let module_specifier = builder.context.owned_unquoted_text(source)?;
    match name_node.kind() {
        "identifier" | "property_identifier" => {
            let local_name = builder.context.owned_text(name_node)?;
            let (kind, imported_name, span_node) = match selected_member {
                Some(member) => (
                    ImportBindingKind::Named,
                    builder.context.owned_unquoted_text(member)?,
                    member,
                ),
                None => (ImportBindingKind::Namespace, "*".to_owned(), name_node),
            };
            builder.emit_import_binding(ExtractedImportBinding {
                kind,
                module_specifier,
                imported_name,
                local_name,
                span: span_for(span_node)?,
            })?;
        }
        "object_pattern" if selected_member.is_none() => {
            capture_import_destructuring(
                builder,
                name_node,
                ImportDestructuringInput {
                    module_specifier: &module_specifier,
                    excluded_members: if dynamic_import_is_awaited(call) {
                        &[]
                    } else {
                        &DYNAMIC_IMPORT_PROMISE_METHODS
                    },
                },
            )?;
            // Static destructuring skips the initializer's ordinary walk, so
            // retain the module load even when every selected name is excluded.
            capture_dynamic_import(builder, call)?;
        }
        _ => {
            // Other patterns also skip their initializer walk once the value
            // is classified as a static import. Keep its site-level facts.
            capture_dynamic_import(builder, call)?;
        }
    }
    Ok(())
}

pub(super) fn is_static_module_binding_value(
    builder: &ExtractionBuilder<'_, '_>,
    value: Option<Node<'_>>,
) -> bool {
    if !is_javascript_family(builder.context.snapshot.language()) {
        return false;
    }
    let commonjs = value
        .and_then(commonjs_require_shape)
        .is_some_and(|(call, _)| commonjs_require_source(builder, call).is_some());
    commonjs
        || dynamic_import_binding_shape(builder, value)
            .is_some_and(|(call, _)| dynamic_import_source(builder, call).is_some())
}

fn dynamic_import_binding_shape<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    value: Option<Node<'tree>>,
) -> Option<(Node<'tree>, Option<Node<'tree>>)> {
    let mut node = value?;
    for _ in 0..MAX_DYNAMIC_IMPORT_WRAPPER_DEPTH {
        match node.kind() {
            "await_expression"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "parenthesized_expression"
            | "non_null_expression" => {
                node = expression_wrapper_value(node)?;
            }
            "member_expression" => {
                let object = node.child_by_field_name("object")?;
                let property = node.child_by_field_name("property")?;
                let call = unwrapped_dynamic_import_call(object)?;
                let member = dynamic_import_selected_member(builder, call)?;
                return (member == property).then_some((call, Some(member)));
            }
            "call_expression" => {
                return dynamic_import_source_node(node).map(|call| (call, None));
            }
            _ => return None,
        }
    }
    None
}

fn unwrapped_dynamic_import_call(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_DYNAMIC_IMPORT_WRAPPER_DEPTH {
        match node.kind() {
            "await_expression"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "parenthesized_expression"
            | "non_null_expression" => {
                node = expression_wrapper_value(node)?;
            }
            _ => return dynamic_import_source_node(node),
        }
    }
    None
}

fn dynamic_import_source_node(call: Node<'_>) -> Option<Node<'_>> {
    let function = call.child_by_field_name("function")?;
    (call.kind() == "call_expression" && function.kind() == "import").then_some(call)
}

fn expression_wrapper_value(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("expression")
        .or_else(|| node.child_by_field_name("value"))
        .or_else(|| {
            named_children(node).find(|child| !child.is_extra() && child.kind() != "type_arguments")
        })
}

fn parse_commonjs_require<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    value: Option<Node<'tree>>,
) -> Result<Option<CommonJsRequire<'tree>>, ExtractError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some((call, selected_member)) = commonjs_require_shape(value) else {
        return Ok(None);
    };
    let Some(source) = commonjs_require_source(builder, call) else {
        return Ok(None);
    };
    Ok(Some(CommonJsRequire {
        call,
        selected_member,
        module_specifier: builder.context.owned_unquoted_text(source)?,
    }))
}

fn commonjs_require_shape(value: Node<'_>) -> Option<(Node<'_>, Option<Node<'_>>)> {
    if value.kind() == "call_expression" {
        return Some((value, None));
    }
    if value.kind() == "member_expression" {
        let object = value.child_by_field_name("object")?;
        let property = value.child_by_field_name("property")?;
        return Some((object, Some(property)));
    }
    None
}

fn commonjs_require_source<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'tree>,
) -> Option<Node<'tree>> {
    if call.kind() != "call_expression" || !is_require_callee(builder, call) {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    screened_import_source(builder, require_arguments::single_value(arguments)?)
}

/// Record the module loaded by a `require(..)` (or `createRequire` alias)
/// call that is not a recognized variable binding, such as
/// `function load() { require('./polyfill') }`, owned by the enclosing symbol.
///
/// Bound forms (`const x = require('./m')`) are recorded by
/// [`capture_commonjs_require`]; the call reference itself is kept by the
/// generic call extractor.
pub(super) fn capture_require_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<(), ExtractError> {
    if !is_javascript_family(builder.context.snapshot.language())
        || is_static_commonjs_binding_call(builder, call)
    {
        return Ok(());
    }
    let Some(source) = commonjs_require_source(builder, call) else {
        return Ok(());
    };
    let module_specifier = builder.context.owned_unquoted_text(source)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name: module_specifier,
            kind: ReferenceKind::Imports,
            node: call,
        },
    )
}

fn is_require_callee(builder: &ExtractionBuilder<'_, '_>, call: Node<'_>) -> bool {
    let Some(function) = call
        .child_by_field_name("function")
        .filter(|function| function.kind() == "identifier")
    else {
        return false;
    };
    let callee = builder.context.text(function).trim();
    (callee == "require" && !builder.commonjs_shadowing.require)
        || builder.commonjs_shadowing.is_require_alias(callee)
}

/// Record the module facts of `import('..')` calls that sit in type
/// annotations the walker never visits (parameter, return, and class-field
/// types such as `opts?: import('./opts').Options`), owned by the annotated
/// symbol.
///
/// The member binding uses [`INLINE_IMPORT_TYPE_LOCAL_NAME`], which no source
/// identifier can spell: only the span-exact type reference at the member
/// resolves through it, so an inline import type never captures, or makes
/// ambiguous, an ordinary use of the same name elsewhere in the file.
pub(super) fn capture_type_position_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    if !is_javascript_family(builder.context.snapshot.language()) {
        return Ok(());
    }
    for node in descendants_including_root(root) {
        builder.context.ensure_active()?;
        if node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .is_some_and(|function| function.kind() == "import")
        {
            capture_inline_import_type(builder, node, owner)?;
        }
    }
    Ok(())
}

/// One inline `import('./m').Name` type: the owned module import and the
/// site-only binding of `Name`.
fn capture_inline_import_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(source) = dynamic_import_source(builder, call) else {
        return Ok(());
    };
    let module_specifier = builder.context.owned_unquoted_text(source)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name: module_specifier.clone(),
            kind: ReferenceKind::Imports,
            node: call,
        },
    )?;
    let Some(member) = dynamic_import_selected_member(builder, call) else {
        return Ok(());
    };
    let imported_name = builder.context.owned_unquoted_text(member)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier,
        imported_name,
        local_name: INLINE_IMPORT_TYPE_LOCAL_NAME.to_owned(),
        span: span_for(member)?,
    })
}

pub(super) fn is_static_commonjs_binding_call(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> bool {
    if !is_javascript_family(builder.context.snapshot.language())
        || commonjs_require_source(builder, call).is_none()
    {
        return false;
    }
    let Some((declarator, selected_member)) = commonjs_binding_declarator(call) else {
        return false;
    };
    let Some(name) = declarator.child_by_field_name("name") else {
        return false;
    };
    matches!(name.kind(), "identifier" | "property_identifier")
        || (name.kind() == "object_pattern" && selected_member.is_none())
}

/// Emit an exact module reference for a statically named `import()` call.
///
/// Returns true for every JavaScript-family dynamic-import call, including
/// non-literal expressions, so the generic call extractor does not retain the
/// reserved keyword as a misleading callable named `import`.
pub(super) fn capture_dynamic_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<bool, ExtractError> {
    if call.kind() != "call_expression"
        || !is_javascript_family(builder.context.snapshot.language())
    {
        return Ok(false);
    }
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(false);
    };
    if builder.context.text(function).trim() != "import" {
        return Ok(false);
    }
    let Some(source) = dynamic_import_source(builder, call) else {
        return Ok(true);
    };
    let module_specifier = builder.context.owned_unquoted_text(source)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name: module_specifier.clone(),
            kind: ReferenceKind::Imports,
            node: call,
        },
    )?;
    if dynamic_import_selected_member(builder, call).is_none()
        && react_lazy_default_import(builder, call)
    {
        let imported_name = "default".to_owned();
        builder.emit_import_binding(ExtractedImportBinding {
            kind: ImportBindingKind::Default,
            module_specifier,
            imported_name: imported_name.clone(),
            local_name: imported_name.clone(),
            span: span_for(call)?,
        })?;
        emit_imported_name_reference(builder, imported_name, call)?;
    } else if let Some(member) = dynamic_import_selected_member(builder, call) {
        let imported_name = builder.context.owned_unquoted_text(member)?;
        // A type-position member (`type T = import('./m').Name`) is an inline
        // import type: like the unwalked annotations, its binding is site-only.
        let local_name = if is_inline_import_type(member) {
            INLINE_IMPORT_TYPE_LOCAL_NAME.to_owned()
        } else {
            imported_name.clone()
        };
        builder.emit_import_binding(ExtractedImportBinding {
            kind: ImportBindingKind::Named,
            module_specifier,
            imported_name: imported_name.clone(),
            local_name,
            span: span_for(member)?,
        })?;
        emit_imported_name_reference(builder, imported_name, member)?;
    }
    Ok(true)
}

/// Whether a selected `import()` member sits in a type position.
fn is_inline_import_type(member: Node<'_>) -> bool {
    member.parent().is_some_and(|selection| {
        selection
            .parent()
            .is_some_and(|holder| references::holds_type_at(holder, selection))
    })
}

fn react_lazy_default_import(builder: &ExtractionBuilder<'_, '_>, call: Node<'_>) -> bool {
    let mut expression = call;
    for _ in 0..12 {
        let Some(parent) = expression.parent() else {
            return false;
        };
        if matches!(
            parent.kind(),
            "await_expression"
                | "as_expression"
                | "satisfies_expression"
                | "type_assertion"
                | "parenthesized_expression"
                | "non_null_expression"
                | "return_statement"
                | "statement_block"
        ) {
            expression = parent;
            continue;
        }
        if !matches!(parent.kind(), "arrow_function" | "function_expression") {
            return false;
        }
        let Some(arguments) = parent.parent().filter(|node| node.kind() == "arguments") else {
            return false;
        };
        let Some(lazy_call) = arguments
            .parent()
            .filter(|node| node.kind() == "call_expression")
        else {
            return false;
        };
        let Some(function) = lazy_call.child_by_field_name("function") else {
            return false;
        };
        return matches!(builder.context.text(function).trim(), "lazy" | "React.lazy");
    }
    false
}

fn dynamic_import_source<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'tree>,
) -> Option<Node<'tree>> {
    dynamic_import_source_node(call)?;
    let arguments = call.child_by_field_name("arguments")?;
    // `import(specifier, { with: { type: 'json' } })` carries import attributes
    // in an optional second argument; the specifier is always the first.
    let mut values = named_children(arguments).filter(|child| !child.is_extra());
    let value = values.next()?;
    if values.take(MAX_DYNAMIC_IMPORT_ARGUMENTS).count() >= MAX_DYNAMIC_IMPORT_ARGUMENTS {
        return None;
    }
    screened_import_source(builder, value)
}

/// Static, type, dynamic, and `CommonJS` loads share an escape-free specifier.
/// Escapes require decoding before an exact module identity can be established.
pub(super) fn screened_import_source<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    value: Node<'tree>,
) -> Option<Node<'tree>> {
    static_dynamic_import_source(value).filter(|source| {
        let module = super::syntax::unquote(builder.context.text(*source));
        !module.is_empty() && import_literal_is_safe(module)
    })
}

/// A literal import name is kept in its source spelling unless that spelling
/// carries a credential.
pub(super) fn import_literal_is_safe(value: &str) -> bool {
    !specifier_may_carry_credential(value)
}

fn dynamic_import_selected_member<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'tree>,
) -> Option<Node<'tree>> {
    let mut expression = call;
    let awaited = dynamic_import_is_awaited(call);
    for _ in 0..MAX_DYNAMIC_IMPORT_WRAPPER_DEPTH {
        let parent = expression.parent()?;
        if parent.kind() == "member_expression" && field_matches(parent, "object", expression) {
            return parent.child_by_field_name("property").filter(|property| {
                matches!(
                    property.kind(),
                    "identifier" | "property_identifier" | "type_identifier"
                ) && (awaited
                    || is_inline_import_type(*property)
                    || !DYNAMIC_IMPORT_PROMISE_METHODS.contains(&builder.context.text(*property)))
            });
        }
        if matches!(
            parent.kind(),
            "await_expression"
                | "as_expression"
                | "satisfies_expression"
                | "type_assertion"
                | "parenthesized_expression"
                | "non_null_expression"
        ) {
            expression = parent;
            continue;
        }
        return None;
    }
    None
}

/// An await outside a member access awaits that member, not the import promise.
fn dynamic_import_is_awaited(mut expression: Node<'_>) -> bool {
    for _ in 0..MAX_DYNAMIC_IMPORT_WRAPPER_DEPTH {
        let Some(parent) = expression.parent() else {
            return false;
        };
        match parent.kind() {
            "await_expression" => return true,
            "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "parenthesized_expression"
            | "non_null_expression" => expression = parent,
            _ => return false,
        }
    }
    false
}

/// The static specifier of a module-loading call argument: a string or a
/// template without substitutions, possibly wrapped in a cast or
/// parentheses.
pub(super) fn static_dynamic_import_source(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_DYNAMIC_IMPORT_WRAPPER_DEPTH {
        match node.kind() {
            "string" => return Some(node),
            "template_string" => {
                return (!has_child_kind(node, "template_substitution")).then_some(node);
            }
            "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "parenthesized_expression"
            | "non_null_expression" => {
                node = expression_wrapper_value(node)?;
            }
            _ => return None,
        }
    }
    None
}

fn commonjs_binding_declarator(call: Node<'_>) -> Option<(Node<'_>, Option<Node<'_>>)> {
    let parent = call.parent()?;
    if parent.kind() == "variable_declarator" && field_matches(parent, "value", call) {
        return Some((parent, None));
    }
    if parent.kind() != "member_expression" || !field_matches(parent, "object", call) {
        return None;
    }
    let selected_member = parent.child_by_field_name("property")?;
    let declarator = parent.parent()?;
    (declarator.kind() == "variable_declarator" && field_matches(declarator, "value", parent))
        .then_some((declarator, Some(selected_member)))
}

fn field_matches(parent: Node<'_>, field: &str, expected: Node<'_>) -> bool {
    parent.child_by_field_name(field).is_some_and(|node| {
        node.start_byte() == expected.start_byte() && node.end_byte() == expected.end_byte()
    })
}

fn capture_commonjs_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    name_node: Node<'_>,
    require: &CommonJsRequire<'_>,
) -> Result<(), ExtractError> {
    match name_node.kind() {
        "identifier" | "property_identifier" => {
            capture_commonjs_identifier_binding(builder, name_node, require)
        }
        "object_pattern" if require.selected_member.is_none() => capture_import_destructuring(
            builder,
            name_node,
            ImportDestructuringInput {
                module_specifier: &require.module_specifier,
                excluded_members: &[],
            },
        ),
        _ => Ok(()),
    }
}

fn capture_commonjs_identifier_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    name_node: Node<'_>,
    require: &CommonJsRequire<'_>,
) -> Result<(), ExtractError> {
    let local_name = builder.context.owned_text(name_node)?;
    let (kind, imported_name, binding_node) = match require.selected_member {
        Some(member) => (
            ImportBindingKind::Named,
            builder.context.owned_text(member)?,
            member,
        ),
        None => (ImportBindingKind::Namespace, "*".to_owned(), name_node),
    };
    builder.emit_import_binding(ExtractedImportBinding {
        kind,
        module_specifier: require.module_specifier.clone(),
        imported_name: imported_name.clone(),
        local_name,
        span: span_for(binding_node)?,
    })?;
    if kind == ImportBindingKind::Named {
        emit_imported_name_reference(builder, imported_name, binding_node)?;
    }
    Ok(())
}

fn emit_commonjs_module_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    require: CommonJsRequire<'_>,
) -> Result<(), ExtractError> {
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: require.module_specifier,
            kind: ReferenceKind::Imports,
            node: require.call,
        },
    )
}

fn capture_import_destructuring(
    builder: &mut ExtractionBuilder<'_, '_>,
    pattern: Node<'_>,
    input: ImportDestructuringInput<'_>,
) -> Result<(), ExtractError> {
    for child in named_children(pattern) {
        let (imported_node, local_node) = match child.kind() {
            "shorthand_property_identifier_pattern" => (child, child),
            "pair_pattern" => {
                let Some(key) = child.child_by_field_name("key") else {
                    continue;
                };
                let Some(value) = child.child_by_field_name("value") else {
                    continue;
                };
                if !matches!(value.kind(), "identifier" | "property_identifier") {
                    continue;
                }
                (key, value)
            }
            _ => continue,
        };
        let imported_name = builder.context.owned_unquoted_text(imported_node)?;
        if input.excluded_members.contains(&imported_name.as_str())
            || !import_literal_is_safe(&imported_name)
        {
            continue;
        }
        let local_name = builder.context.owned_text(local_node)?;
        builder.emit_import_binding(ExtractedImportBinding {
            kind: ImportBindingKind::Named,
            module_specifier: input.module_specifier.to_owned(),
            imported_name: imported_name.clone(),
            local_name,
            span: span_for(imported_node)?,
        })?;
        emit_imported_name_reference(builder, imported_name, imported_node)?;
    }
    Ok(())
}

fn emit_imported_name_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    name: String,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name,
            kind: ReferenceKind::References,
            node,
        },
    )
}

pub(super) fn capture_commonjs_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "assignment_expression"
        || !is_javascript_family(builder.context.snapshot.language())
        || !is_top_level_assignment(node)
    {
        return Ok(());
    }
    let Some(left) = node.child_by_field_name("left") else {
        return Ok(());
    };
    let Some(right) = node.child_by_field_name("right") else {
        return Ok(());
    };
    let left_text = builder.context.text(left).trim();
    if commonjs_export_root_shadowed(builder, left_text) {
        return Ok(());
    }
    if left_text == "module.exports" && right.kind() == "object" {
        return capture_commonjs_object_exports(builder, right);
    }
    let Some(public_name) = commonjs_member_export_name(left_text).map(str::to_owned) else {
        return Ok(());
    };
    if !is_plain_identifier(right) {
        return Ok(());
    }
    let local_name = builder.context.owned_text(right)?;
    if public_name != local_name {
        emit_export_alias(
            builder,
            ExportAlias {
                public_name,
                local_name,
                span_node: left,
                reference_node: right,
                source: None,
            },
        )?;
    }
    Ok(())
}

fn capture_commonjs_object_exports(
    builder: &mut ExtractionBuilder<'_, '_>,
    object: Node<'_>,
) -> Result<(), ExtractError> {
    for child in named_children(object) {
        builder.context.ensure_active()?;
        if child.kind() != "pair" {
            continue;
        }
        let Some(public_node) = child.child_by_field_name("key") else {
            continue;
        };
        let Some(local_node) = child.child_by_field_name("value") else {
            continue;
        };
        if !is_static_export_name(public_node) || !is_plain_identifier(local_node) {
            continue;
        }
        let public_name = builder.context.owned_unquoted_text(public_node)?;
        let local_name = builder.context.owned_text(local_node)?;
        if public_name == local_name {
            continue;
        }
        emit_export_alias(
            builder,
            ExportAlias {
                public_name,
                local_name,
                span_node: child,
                reference_node: local_node,
                source: None,
            },
        )?;
    }
    Ok(())
}

pub(super) fn emit_export_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    alias: ExportAlias<'_>,
) -> Result<(), ExtractError> {
    if !import_literal_is_safe(&alias.public_name) || !import_literal_is_safe(&alias.local_name) {
        return Ok(());
    }
    let owner = emit_export_alias_symbol(builder, &alias)?;
    emit_export_alias_binding(builder, &alias)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner),
            name: alias.local_name,
            kind: ReferenceKind::Exports,
            node: alias.reference_node,
        },
    )
}

pub(super) fn emit_namespace_reexport(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamespaceReexportInput<'_>,
) -> Result<(), ExtractError> {
    let NamespaceReexportInput {
        namespace_node,
        public_node,
        module_specifier,
    } = input;
    let public_name = builder.context.owned_unquoted_text(public_node)?;
    if !import_literal_is_safe(&public_name) {
        return Ok(());
    }
    let alias = ExportAlias {
        public_name: public_name.clone(),
        local_name: public_name.clone(),
        span_node: namespace_node,
        reference_node: public_node,
        source: None,
    };
    emit_export_alias_symbol(builder, &alias)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::ReExportNamespace,
        module_specifier,
        imported_name: "*".to_owned(),
        local_name: public_name,
        span: span_for(public_node)?,
    })
}

pub(super) struct NamespaceReexportInput<'tree> {
    namespace_node: Node<'tree>,
    public_node: Node<'tree>,
    module_specifier: String,
}

impl<'tree> NamespaceReexportInput<'tree> {
    pub(super) const fn new(
        namespace_node: Node<'tree>,
        public_node: Node<'tree>,
        module_specifier: String,
    ) -> Self {
        Self {
            namespace_node,
            public_node,
            module_specifier,
        }
    }
}

fn emit_export_alias_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    alias: &ExportAlias<'_>,
) -> Result<cartograph_domain::SymbolId, ExtractError> {
    let pending = PendingSymbol {
        kind: SymbolKind::Export,
        name: alias.public_name.clone(),
        span_node: alias.span_node,
        structural_node: alias.span_node,
        doc_anchor: alias.span_node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(true, alias.public_name == "default"),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(pending)
}

fn emit_export_alias_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    alias: &ExportAlias<'_>,
) -> Result<(), ExtractError> {
    let Some(module_specifier) = &alias.source else {
        return Ok(());
    };
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier: module_specifier.clone(),
        imported_name: alias.local_name.clone(),
        local_name: alias.local_name.clone(),
        span: span_for(alias.reference_node)?,
    })
}

fn collect_local_export_statement(
    builder: &mut ExtractionBuilder<'_, '_>,
    statement: Node<'_>,
) -> Result<(), ExtractError> {
    for child in named_children(statement) {
        if child.kind() != "export_clause" {
            continue;
        }
        for specifier in named_children(child) {
            if specifier.kind() != "export_specifier" {
                continue;
            }
            let Some(name_node) = specifier.child_by_field_name("name") else {
                continue;
            };
            let alias = specifier.child_by_field_name("alias");
            if alias.is_none() {
                insert_explicit_export(builder, name_node, false)?;
            }
        }
    }
    if has_child_kind(statement, "default")
        && let Some(identifier) =
            named_children(statement).find(|child| child.kind() == "identifier")
    {
        insert_explicit_export(builder, identifier, true)?;
    }
    Ok(())
}

fn collect_commonjs_export_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    assignment: Node<'_>,
) -> Result<(), ExtractError> {
    if !is_top_level_assignment(assignment) {
        return Ok(());
    }
    let Some(left) = assignment.child_by_field_name("left") else {
        return Ok(());
    };
    let Some(right) = assignment.child_by_field_name("right") else {
        return Ok(());
    };
    let left_text = builder.context.text(left).trim();
    if commonjs_export_root_shadowed(builder, left_text) {
        return Ok(());
    }
    if left_text == "module.exports" {
        return collect_commonjs_module_exports(builder, right);
    }
    let Some(public_name) = commonjs_member_export_name(left_text) else {
        return Ok(());
    };
    if is_plain_identifier(right) && builder.context.text(right).trim() == public_name {
        insert_explicit_export(builder, right, false)?;
    }
    Ok(())
}

fn collect_commonjs_module_exports(
    builder: &mut ExtractionBuilder<'_, '_>,
    right: Node<'_>,
) -> Result<(), ExtractError> {
    if is_plain_identifier(right) {
        return insert_explicit_export(builder, right, true);
    }
    if right.kind() != "object" {
        return Ok(());
    }
    for child in named_children(right) {
        match child.kind() {
            "shorthand_property_identifier" => {
                insert_explicit_export(builder, child, false)?;
            }
            "pair" => collect_commonjs_pair_export(builder, child)?,
            _ => {}
        }
    }
    Ok(())
}

fn collect_commonjs_pair_export(
    builder: &mut ExtractionBuilder<'_, '_>,
    pair: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(key) = pair.child_by_field_name("key") else {
        return Ok(());
    };
    let Some(value) = pair.child_by_field_name("value") else {
        return Ok(());
    };
    if is_plain_identifier(value)
        && builder.context.text(key).trim() == builder.context.text(value).trim()
    {
        insert_explicit_export(builder, value, false)?;
    }
    Ok(())
}

fn insert_explicit_export(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    default_export: bool,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(node)?;
    builder.explicit_exports.insert(name.clone());
    if default_export {
        builder.explicit_default_exports.insert(name);
    }
    Ok(())
}

fn commonjs_member_export_name(left: &str) -> Option<&str> {
    left.strip_prefix("exports.")
        .or_else(|| left.strip_prefix("module.exports."))
        .filter(|name| !name.is_empty() && !name.contains('.'))
}

fn commonjs_export_root_shadowed(builder: &ExtractionBuilder<'_, '_>, left: &str) -> bool {
    (left.starts_with("module.exports") && builder.commonjs_shadowing.module)
        || (left.starts_with("exports.") && builder.commonjs_shadowing.exports)
}

fn is_plain_identifier(node: Node<'_>) -> bool {
    matches!(node.kind(), "identifier" | "shorthand_property_identifier")
}

fn is_static_export_name(node: Node<'_>) -> bool {
    matches!(node.kind(), "identifier" | "property_identifier" | "string")
}

fn is_top_level_assignment(node: Node<'_>) -> bool {
    node.parent()
        .filter(|parent| parent.kind() == "expression_statement")
        .and_then(|parent| parent.parent())
        .is_some_and(|parent| parent.kind() == "program")
}

/// Languages with ECMAScript module syntax; `ArkTS` keeps TypeScript's
/// `import`/`export` grammar, so its export clauses mark declarations too.
pub(super) fn is_javascript_family(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::ArkTs
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use tree_sitter::Parser;

    use super::*;
    use crate::{DEFAULT_MAXIMUM_AST_DEPTH, NativeGrammar, SourceLimits, SourceSnapshot};

    const COMPLEX_COMMONJS_EXPORTS: usize = 2_048;

    #[test]
    fn commonjs_object_export_capture_polls_before_skipping_complex_values() {
        let mut source = String::from("module.exports = {\n");
        for index in 0..COMPLEX_COMMONJS_EXPORTS {
            assert!(
                writeln!(&mut source, "key_{index}: factory(value_{index}),").is_ok(),
                "writing to a String is infallible"
            );
        }
        source.push_str("};\n");

        let limits = SourceLimits::new(source.len())
            .unwrap_or_else(|error| panic!("test source bound is invalid: {error}"));
        let snapshot = SourceSnapshot::from_bytes("src/cancel.js", source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("test snapshot is invalid: {error}"));
        let mut parser = Parser::new();
        parser
            .set_language(&NativeGrammar::JavaScript.language())
            .unwrap_or_else(|error| panic!("test grammar setup failed: {error}"));
        let tree = parser
            .parse(snapshot.source(), None)
            .unwrap_or_else(|| panic!("test parser did not produce a tree"));
        let object = descendants_including_root(tree.root_node())
            .find(|node| node.kind() == "object")
            .unwrap_or_else(|| panic!("test CommonJS object was not found"));

        let mut polls = 0_usize;
        let mut cancelled = || {
            polls = polls.saturating_add(1);
            true
        };
        let mut builder =
            ExtractionBuilder::new(&snapshot, DEFAULT_MAXIMUM_AST_DEPTH, &mut cancelled)
                .unwrap_or_else(|error| panic!("test extraction builder failed: {error}"));
        let result = capture_commonjs_object_exports(&mut builder, object);
        drop(builder);

        assert_eq!(result, Err(ExtractError::Cancelled));
        assert_eq!(polls, 1);
    }
}
