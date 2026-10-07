//! Conservative alias proofs: one declaration, no rebinding or shadow anywhere.
//! Repeated lexical names abstain, leaving normal language resolution intact.

use tree_sitter::Node;

use super::{BTreeMap, ExtractError, FrameworkBuilder, MAX_NATIVE_ALIASES, javascript_shapes};

const ALIAS_INDEX_ENTRY_BYTES: u64 = 128;
const MAX_MEMBER_HOPS: usize = 16;

struct MemberAlias<'tree, 'aliases> {
    name: Node<'tree>,
    value: Node<'tree>,
    unique: &'aliases BTreeMap<String, usize>,
    aliases: &'aliases mut Vec<(String, String)>,
}

pub(super) fn unique_names(
    builder: &mut FrameworkBuilder<'_, '_>,
) -> Result<BTreeMap<String, usize>, ExtractError> {
    let mut names = BTreeMap::new();
    let Some(root) = builder.syntax_root() else {
        return Ok(names);
    };
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        builder.bridge.charge_work(1)?;
        if dynamic_bindings(builder, node) {
            return Ok(BTreeMap::new());
        }
        if (is_binding(node) || is_written_receiver(node)) && !framework_import(builder, node) {
            let text = builder.source().get(node.byte_range()).unwrap_or_default();
            builder.bridge.reserve_working_bytes(
                ALIAS_INDEX_ENTRY_BYTES
                    + u64::try_from(text.len()).map_err(|_| ExtractError::OutputLimit)?,
            )?;
            *names.entry(text.to_owned()).or_default() += 1;
        }
        if !advance(builder, &mut cursor)? {
            return Ok(names);
        }
    }
}

pub(super) fn advance(
    builder: &mut FrameworkBuilder<'_, '_>,
    cursor: &mut tree_sitter::TreeCursor<'_>,
) -> Result<bool, ExtractError> {
    if cursor.goto_first_child() {
        return Ok(true);
    }
    loop {
        builder.bridge.charge_work(1)?;
        if cursor.goto_next_sibling() {
            return Ok(true);
        }
        if !cursor.goto_parent() {
            return Ok(false);
        }
    }
}

fn is_binding(node: Node<'_>) -> bool {
    if node.kind() == "type_identifier" {
        return node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "class" | "class_declaration" | "abstract_class_declaration"
            ) && parent.child_by_field_name("name") == Some(node)
        });
    }
    if !matches!(
        node.kind(),
        "identifier" | "shorthand_property_identifier_pattern"
    ) {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declarator"
        | "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "class"
        | "generator_function"
        | "function_expression" => parent.child_by_field_name("name") == Some(node),
        "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
            parent.child_by_field_name("left") == Some(node)
        }
        "formal_parameters" | "required_parameter" | "optional_parameter" | "rest_pattern"
        | "object_pattern" | "array_pattern" | "catch_clause" | "update_expression"
        | "import_clause" | "namespace_import" => true,
        "pair_pattern" | "assignment_pattern" | "object_assignment_pattern" => {
            parent
                .child_by_field_name("value")
                .or_else(|| parent.child_by_field_name("left"))
                == Some(node)
        }
        "arrow_function" => parent.child_by_field_name("parameter") == Some(node),
        "import_specifier" => {
            parent
                .child_by_field_name("alias")
                .or_else(|| parent.child_by_field_name("name"))
                == Some(node)
        }
        _ => false,
    }
}

fn is_written_receiver(mut node: Node<'_>) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    for _ in 0..MAX_MEMBER_HOPS {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "member_expression" | "subscript_expression" => {
                if parent.child_by_field_name("object") != Some(node) {
                    return false;
                }
            }
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "non_null_expression"
            | "sequence_expression"
            | "array_pattern"
            | "object_pattern"
            | "pair_pattern"
            | "assignment_pattern"
            | "object_assignment_pattern"
            | "rest_pattern" => {}
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                return parent.child_by_field_name("left") == Some(node);
            }
            "update_expression" => return parent.child_by_field_name("argument") == Some(node),
            "unary_expression" => {
                return parent.child_by_field_name("argument") == Some(node)
                    && parent
                        .child_by_field_name("operator")
                        .is_some_and(|operator| operator.kind() == "delete");
            }
            _ => return false,
        }
        node = parent;
    }
    // An overlong receiver chain cannot prove the absence of a write.
    true
}

pub(super) fn native_modules(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    unique: &BTreeMap<String, usize>,
) -> Result<Vec<(String, String)>, ExtractError> {
    let mut aliases = Vec::new();
    if unique.contains_key("NativeModules") {
        return Ok(aliases);
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(aliases);
    };
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        builder.bridge.charge_work(1)?;
        if node.kind() == "variable_declarator" && javascript_shapes::module_declarator(node) {
            collect_native_alias(builder, source, (node, unique, &mut aliases))?;
        }
        if !advance(builder, &mut cursor)? {
            return Ok(aliases);
        }
    }
}

fn dynamic_bindings(builder: &FrameworkBuilder<'_, '_>, node: Node<'_>) -> bool {
    node.kind() == "with_statement"
        || node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .and_then(javascript_shapes::unwrap)
                .is_none_or(|function| builder.source().get(function.byte_range()) == Some("eval"))
}

fn framework_import(builder: &FrameworkBuilder<'_, '_>, node: Node<'_>) -> bool {
    let Some(specifier) = node
        .parent()
        .filter(|parent| parent.kind() == "import_specifier")
    else {
        return false;
    };
    let name = builder.source().get(node.byte_range()).unwrap_or_default();
    let imported = specifier
        .child_by_field_name("name")
        .and_then(|name| builder.source().get(name.byte_range()));
    if imported != Some(name) {
        return false;
    }
    let source = specifier
        .parent()
        .and_then(|node| node.parent())
        .and_then(|node| node.parent())
        .and_then(|statement| statement.child_by_field_name("source"))
        .and_then(|source| builder.source().get(source.byte_range()));
    matches!(
        (name, source),
        (
            "NativeModules" | "TurboModuleRegistry",
            Some("'react-native'" | "\"react-native\"")
        ) | (
            "requireNativeModule" | "requireOptionalNativeModule",
            Some("'expo-modules-core'" | "\"expo-modules-core\"")
        )
    )
}

fn collect_native_alias(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (node, unique, aliases): (
        Node<'_>,
        &BTreeMap<String, usize>,
        &mut Vec<(String, String)>,
    ),
) -> Result<(), ExtractError> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let Some(value) = node
        .child_by_field_name("value")
        .and_then(javascript_shapes::unwrap)
    else {
        return Ok(());
    };
    if name.kind() == "identifier" && value.kind() == "member_expression" {
        collect_member_alias(
            builder,
            source,
            &mut MemberAlias {
                name,
                value,
                unique,
                aliases,
            },
        )?;
    }
    if name.kind() == "object_pattern" && source.get(value.byte_range()) == Some("NativeModules") {
        collect_destructured_aliases(builder, source, (name, unique, aliases))?;
    }
    Ok(())
}

fn collect_member_alias(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    input: &mut MemberAlias<'_, '_>,
) -> Result<(), ExtractError> {
    let object = input
        .value
        .child_by_field_name("object")
        .and_then(|node| source.get(node.byte_range()));
    if object != Some("NativeModules") {
        return Ok(());
    }
    if let Some(module) = input
        .value
        .child_by_field_name("property")
        .and_then(|node| source.get(node.byte_range()))
    {
        push_alias(
            builder,
            input.aliases,
            (source, input.name, module, input.unique),
        )?;
    }
    Ok(())
}

fn collect_destructured_aliases(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (pattern, unique, aliases): (
        Node<'_>,
        &BTreeMap<String, usize>,
        &mut Vec<(String, String)>,
    ),
) -> Result<(), ExtractError> {
    let mut cursor = pattern.walk();
    for property in pattern.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        let (key, local) = if property.kind() == "pair_pattern" {
            (
                property.child_by_field_name("key"),
                property.child_by_field_name("value"),
            )
        } else {
            (Some(property), Some(property))
        };
        if let Some((key, local)) = key.zip(local)
            && let Some(module) = source.get(key.byte_range())
        {
            push_alias(builder, aliases, (source, local, module, unique))?;
        }
    }
    Ok(())
}

fn push_alias(
    builder: &mut FrameworkBuilder<'_, '_>,
    aliases: &mut Vec<(String, String)>,
    (source, local, module, unique): (&str, Node<'_>, &str, &BTreeMap<String, usize>),
) -> Result<(), ExtractError> {
    if !matches!(
        local.kind(),
        "identifier" | "shorthand_property_identifier_pattern"
    ) {
        return Ok(());
    }
    let Some(alias) = source.get(local.byte_range()) else {
        return Ok(());
    };
    if unique.get(alias) != Some(&1) || aliases.len() >= MAX_NATIVE_ALIASES {
        return Ok(());
    }
    builder.bridge.reserve_working_bytes(
        ALIAS_INDEX_ENTRY_BYTES
            + u64::try_from(alias.len() + module.len()).map_err(|_| ExtractError::OutputLimit)?,
    )?;
    aliases.push((alias.to_owned(), module.to_owned()));
    Ok(())
}
