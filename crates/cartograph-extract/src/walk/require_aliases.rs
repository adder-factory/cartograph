//! `createRequire` aliases: ES modules that load `CommonJS` modules through
//! `const load = createRequire(import.meta.url)`.
//!
//! A top-level binding is an alias only when its initializer calls the real
//! Node.js factory: a `createRequire` imported (possibly renamed) from
//! `module`/`node:module`, or the `createRequire` member of that module object.
//! A callee that is merely *named* `createRequire` proves nothing, and a name
//! such as `requireAuth` is never an alias.

use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::ExtractError;

use super::{ExtractionBuilder, javascript_reads, syntax::named_children};

/// Node.js module specifiers that export `createRequire`.
const NODE_MODULE_SPECIFIERS: &[&str] = &["module", "node:module"];
/// The Node.js factory that creates a `require` function.
const CREATE_REQUIRE: &str = "createRequire";

/// Local names that refer to the Node.js module or its factory.
#[derive(Default)]
struct NodeModuleBindings {
    /// Locals bound to the `createRequire` function itself.
    factories: BTreeSet<String>,
    /// Locals bound to the whole `module` object.
    modules: BTreeSet<String>,
}

/// The `createRequire` aliases of one file and the factory/module locals they
/// were proven through.
#[derive(Default)]
pub(super) struct RequireAliases {
    /// Top-level bindings created by the Node.js factory.
    pub(super) aliases: BTreeSet<String>,
    /// Locals bound to the factory or the module object. Rebinding or writing
    /// any of them withdraws the proof for every alias.
    pub(super) fences: BTreeSet<String>,
}

/// Collect the top-level bindings created by the Node.js `createRequire`.
pub(super) fn collect(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<RequireAliases, ExtractError> {
    let mut bindings = NodeModuleBindings::default();
    for statement in named_children(root) {
        builder.context.ensure_active()?;
        if statement.kind() == "import_statement" {
            record_import(builder, statement, &mut bindings)?;
        }
    }
    let mut collected = RequireAliases::default();
    for declarator in javascript_reads::module_declarators(root) {
        builder.context.ensure_active()?;
        record_commonjs_module(builder, declarator, &mut bindings)?;
        if let Some(alias) = factory_call_alias(builder, declarator, &bindings)? {
            collected.aliases.insert(alias);
        }
    }
    if !collected.aliases.is_empty() {
        collected.fences = bindings.factories;
        collected.fences.append(&mut bindings.modules);
    }
    Ok(collected)
}

/// Whether a module specifier names the Node.js `module` builtin.
fn is_node_module_specifier(builder: &ExtractionBuilder<'_, '_>, source: Node<'_>) -> bool {
    NODE_MODULE_SPECIFIERS.contains(&super::syntax::unquote(builder.context.text(source)))
}

/// `import { createRequire [as cr] } from 'module'` and
/// `import * as module / import Module from 'node:module'`.
fn record_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    statement: Node<'_>,
    bindings: &mut NodeModuleBindings,
) -> Result<(), ExtractError> {
    if !statement
        .child_by_field_name("source")
        .is_some_and(|source| is_node_module_specifier(builder, source))
    {
        return Ok(());
    }
    let Some(clause) = named_children(statement).find(|child| child.kind() == "import_clause")
    else {
        return Ok(());
    };
    for child in named_children(clause) {
        match child.kind() {
            "identifier" => {
                bindings.modules.insert(builder.context.owned_text(child)?);
            }
            "namespace_import" => {
                if let Some(local) = child.named_child(0) {
                    bindings.modules.insert(builder.context.owned_text(local)?);
                }
            }
            "named_imports" => record_named_factories(builder, child, bindings)?,
            _ => {}
        }
    }
    Ok(())
}

/// Record the (possibly renamed) `createRequire` named imports.
fn record_named_factories(
    builder: &mut ExtractionBuilder<'_, '_>,
    named_imports: Node<'_>,
    bindings: &mut NodeModuleBindings,
) -> Result<(), ExtractError> {
    for specifier in
        named_children(named_imports).filter(|child| child.kind() == "import_specifier")
    {
        let Some(name) = specifier.child_by_field_name("name") else {
            continue;
        };
        if super::syntax::unquote(builder.context.text(name)) != CREATE_REQUIRE {
            continue;
        }
        let local = specifier.child_by_field_name("alias").unwrap_or(name);
        bindings
            .factories
            .insert(builder.context.owned_text(local)?);
    }
    Ok(())
}

/// `const { createRequire [: cr] } = require('module')` and
/// `const Module = require('node:module')`.
fn record_commonjs_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    declarator: Node<'_>,
    bindings: &mut NodeModuleBindings,
) -> Result<(), ExtractError> {
    let (Some(name), Some(value)) = (
        declarator.child_by_field_name("name"),
        declarator.child_by_field_name("value"),
    ) else {
        return Ok(());
    };
    if !requires_node_module(builder, value) {
        return Ok(());
    }
    match name.kind() {
        "identifier" => {
            bindings.modules.insert(builder.context.owned_text(name)?);
        }
        "object_pattern" => {
            for property in named_children(name) {
                if let Some(local) = destructured_factory(builder, property) {
                    bindings
                        .factories
                        .insert(builder.context.owned_text(local)?);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The local bound to `createRequire` by one destructuring property.
fn destructured_factory<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    property: Node<'tree>,
) -> Option<Node<'tree>> {
    match property.kind() {
        "shorthand_property_identifier_pattern" => {
            (builder.context.text(property) == CREATE_REQUIRE).then_some(property)
        }
        "pair_pattern" => {
            let key = property.child_by_field_name("key")?;
            let value = property.child_by_field_name("value")?;
            (super::syntax::unquote(builder.context.text(key)) == CREATE_REQUIRE
                && value.kind() == "identifier")
                .then_some(value)
        }
        _ => None,
    }
}

/// Whether `value` loads the Node.js `module` builtin through `CommonJS`.
fn requires_node_module(builder: &ExtractionBuilder<'_, '_>, value: Node<'_>) -> bool {
    !builder.commonjs_shadowing.shadows_require()
        && value.kind() == "call_expression"
        && value
            .child_by_field_name("function")
            .is_some_and(|function| builder.context.text(function).trim() == "require")
        && value
            .child_by_field_name("arguments")
            .filter(|arguments| arguments.named_child_count() == 1)
            .and_then(|arguments| arguments.named_child(0))
            .and_then(super::module_system::static_dynamic_import_source)
            .is_some_and(|source| is_node_module_specifier(builder, source))
}

/// The bound name of `const alias = createRequire(..)` when the callee is
/// the Node.js factory.
fn factory_call_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    declarator: Node<'_>,
    bindings: &NodeModuleBindings,
) -> Result<Option<String>, ExtractError> {
    let Some(name) = declarator
        .child_by_field_name("name")
        .filter(|name| name.kind() == "identifier")
    else {
        return Ok(None);
    };
    let Some(callee) = declarator
        .child_by_field_name("value")
        .filter(|value| value.kind() == "call_expression")
        .and_then(|call| call.child_by_field_name("function"))
    else {
        return Ok(None);
    };
    let is_factory = match callee.kind() {
        "identifier" => bindings.factories.contains(builder.context.text(callee)),
        "member_expression" => {
            callee
                .child_by_field_name("object")
                .filter(|object| object.kind() == "identifier")
                .is_some_and(|object| bindings.modules.contains(builder.context.text(object)))
                && callee
                    .child_by_field_name("property")
                    .is_some_and(|property| builder.context.text(property) == CREATE_REQUIRE)
        }
        _ => false,
    };
    if !is_factory {
        return Ok(None);
    }
    builder.context.owned_text(name).map(Some)
}
