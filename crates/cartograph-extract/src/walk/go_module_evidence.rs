//! Carry package qualifiers without changing the captured reference identity.
use super::{
    ExtractionBuilder, MAX_BOUNDED_AST_VISITS, polyglot::qualified_path,
    syntax::descendants_including_root,
};
use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};
use cartograph_domain::{ReferenceKind, SourceLanguage};
use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
};
use tree_sitter::Node;

const MAP_ALLOWANCE: usize = 128;
const METADATA: &str = "<go-package-site>";

pub(super) fn enrich(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Go || root.has_error() {
        return Ok(());
    }
    let packages = packages(builder)?;
    let sites = sites(builder, root, &packages)?;
    for position in 0..builder.facts.references.len() {
        builder.context.ensure_active()?;
        let reference = &builder.facts.references[position];
        if !matches!(
            reference.kind,
            ReferenceKind::FieldAccess | ReferenceKind::TypeOf | ReferenceKind::Returns
        ) || reference.resolution_name.is_some()
        {
            continue;
        }
        if let Some(path) = sites.get(&(reference.span.start_byte(), reference.span.end_byte())) {
            let binding = ExtractedImportBinding {
                kind: ImportBindingKind::Named,
                module_specifier: METADATA.to_owned(),
                imported_name: builder.context.copy_text(path)?,
                local_name: reference.kind.as_str().to_owned(),
                span: reference.span,
            };
            builder.emit_import_binding(binding)?;
        }
    }
    Ok(())
}

fn packages(builder: &mut ExtractionBuilder<'_, '_>) -> Result<HashSet<String>, ExtractError> {
    let mut packages = HashSet::new();
    for binding in &builder.facts.import_bindings {
        builder.context.ensure_active()?;
        if binding.kind == ImportBindingKind::Namespace
            && !matches!(binding.local_name.as_str(), "*" | "." | "_")
        {
            builder.context.budget.reserve_working_bytes(
                u64::try_from(MAP_ALLOWANCE + size_of::<String>() + binding.local_name.len())
                    .map_err(|_| ExtractError::OutputLimit)?,
            )?;
            packages
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            packages.insert(builder.context.copy_text(&binding.local_name)?);
        }
    }
    Ok(packages)
}

fn sites(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    packages: &HashSet<String>,
) -> Result<HashMap<(u64, u64), String>, ExtractError> {
    let mut sites = HashMap::new();
    for (visited, node) in descendants_including_root(root).enumerate() {
        builder.context.ensure_active()?;
        if visited >= MAX_BOUNDED_AST_VISITS {
            return Err(ExtractError::NestingLimit);
        }
        let Some((package, leaf)) = qualified_site(node) else {
            continue;
        };
        if !packages.contains(builder.context.text(package).trim()) {
            continue;
        }
        let Some(path) = qualified_path::lookup_name(builder, node)? else {
            continue;
        };
        builder.context.budget.reserve_working_bytes(
            u64::try_from(MAP_ALLOWANCE + size_of::<((u64, u64), String)>() + path.len())
                .map_err(|_| ExtractError::OutputLimit)?,
        )?;
        sites
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        let span = super::syntax::span_for(leaf)?;
        sites.insert((span.start_byte(), span.end_byte()), path);
    }
    Ok(sites)
}

fn qualified_site(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let fields = match node.kind() {
        "qualified_type" => ("package", "name"),
        "selector_expression" => ("operand", "field"),
        _ => return None,
    };
    node.child_by_field_name(fields.0)
        .filter(|package| matches!(package.kind(), "package_identifier" | "identifier"))
        .zip(node.child_by_field_name(fields.1))
}
