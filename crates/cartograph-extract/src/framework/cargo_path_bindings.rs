//! Only unconditional, literal Cargo path dependencies supply local crate edges.
use super::FrameworkBuilder;
use crate::{
    ExtractError, ExtractedImportBinding, ImportBindingKind, budget::import_binding_budget_bytes,
    source_lines::SourceByteRange,
};
use cartograph_domain::{SourceLanguage, SourceSpan};
use toml_edit::{Document, Item, Value};

const MAXIMUM_MANIFEST_BYTES: usize = 256 * 1_024;
const MAXIMUM_BINDING_BYTES: usize = 1_024;
const PARSE_BYTES_PER_SOURCE_BYTE: u64 = 64;

pub(super) fn extract(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let snapshot = builder.snapshot;
    if snapshot.language() != SourceLanguage::Toml
        || snapshot.path().as_str().rsplit('/').next() != Some("Cargo.toml")
        || snapshot.source().len() > MAXIMUM_MANIFEST_BYTES
    {
        return Ok(());
    }
    builder.bridge.check_cancelled()?;
    builder.bridge.budget.reserve_working_bytes(
        u64::try_from(snapshot.source().len())
            .map_err(|_| ExtractError::OutputLimit)?
            .saturating_mul(PARSE_BYTES_PER_SOURCE_BYTE),
    )?;
    let Ok(document) = Document::parse(snapshot.source()) else {
        return Ok(());
    };
    builder.bridge.check_cancelled()?;
    if let Some(package) = document.get("package").and_then(Item::as_table)
        && let Some(name) = package
            .get("name")
            .and_then(Item::as_str)
            .filter(|name| crate_name(name))
        && conventional_library(&document)
        && let Some(range) = package.key("name").and_then(toml_edit::Key::span)
    {
        let span = builder.lines.span(SourceByteRange::new(
            range.start,
            range.end,
            snapshot.source().len(),
        ))?;
        append_binding(
            builder,
            ExtractedImportBinding {
                kind: ImportBindingKind::Namespace,
                module_specifier: "<cargo-conventional-library>".to_owned(),
                imported_name: name.to_owned(),
                local_name: "<cargo-library>".to_owned(),
                span,
            },
        )?;
    }
    target_roots(builder, &document)?;
    let Some(dependencies) = document.get("dependencies").and_then(Item::as_table) else {
        return Ok(());
    };
    for (alias, item) in dependencies {
        builder.bridge.check_cancelled()?;
        let Some(range) = dependencies.key(alias).and_then(toml_edit::Key::span) else {
            continue;
        };
        let span = builder.lines.span(SourceByteRange::new(
            range.start,
            range.end,
            snapshot.source().len(),
        ))?;
        let Some(binding) = path_binding((alias, item), span) else {
            continue;
        };
        append_binding(builder, binding)?;
    }
    Ok(())
}

fn append_binding(
    builder: &mut FrameworkBuilder<'_, '_>,
    binding: ExtractedImportBinding,
) -> Result<(), ExtractError> {
    builder.bridge.budget.reserve_fact(
        import_binding_budget_bytes(&binding),
        [
            binding.module_specifier.as_str(),
            binding.local_name.as_str(),
            binding.imported_name.as_str(),
        ],
    )?;
    builder
        .bridge
        .file
        .import_bindings
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.bridge.file.import_bindings.push(binding);
    Ok(())
}

fn conventional_library(document: &Document<&str>) -> bool {
    if let Some(library) = document.get("lib") {
        return library.get("path").is_none() && library.get("name").is_none();
    }
    document
        .get("package")
        .and_then(Item::as_table)
        .and_then(|table| table.get("autolib"))
        .and_then(Item::as_bool)
        != Some(false)
}

fn target_roots(
    builder: &mut FrameworkBuilder<'_, '_>,
    document: &Document<&str>,
) -> Result<(), ExtractError> {
    let Some(package) = document.get("package").and_then(Item::as_table) else {
        return Ok(());
    };
    let Some(range) = package.key("name").and_then(toml_edit::Key::span) else {
        return Ok(());
    };
    let span = builder.lines.span(SourceByteRange::new(
        range.start,
        range.end,
        builder.snapshot.source().len(),
    ))?;
    if package.get("autobins").and_then(Item::as_bool) != Some(false) {
        target_binding(builder, ("<cargo-auto-bins>", "."), span)?;
    }
    if let Some(library) = document.get("lib") {
        let path = library
            .get("path")
            .and_then(Item::as_str)
            .unwrap_or("src/lib.rs");
        target_binding(builder, ("<cargo-target-root>", path), span)?;
    }
    let Some(binaries) = document.get("bin").and_then(Item::as_array_of_tables) else {
        return Ok(());
    };
    for binary in binaries {
        builder.bridge.check_cancelled()?;
        if let Some(path) = binary.get("path").and_then(Item::as_str) {
            target_binding(builder, ("<cargo-target-root>", path), span)?;
        } else if let Some(name) = binary
            .get("name")
            .and_then(Item::as_str)
            .filter(|name| crate_name(name))
        {
            for path in [
                format!("src/bin/{name}.rs"),
                format!("src/bin/{name}/main.rs"),
            ] {
                target_binding(builder, ("<cargo-target-root>", &path), span)?;
            }
        }
    }
    Ok(())
}

fn target_binding(
    builder: &mut FrameworkBuilder<'_, '_>,
    target: (&str, &str),
    span: SourceSpan,
) -> Result<(), ExtractError> {
    let (marker, path) = target;
    if path.is_empty()
        || path.len() > MAXIMUM_BINDING_BYTES
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
    {
        return Ok(());
    }
    append_binding(
        builder,
        ExtractedImportBinding {
            kind: ImportBindingKind::Namespace,
            module_specifier: path.to_owned(),
            local_name: marker.to_owned(),
            imported_name: marker.to_owned(),
            span,
        },
    )
}

fn path_binding(dependency: (&str, &Item), span: SourceSpan) -> Option<ExtractedImportBinding> {
    let (alias, item) = dependency;
    let package = string_field(item, "package").unwrap_or(alias);
    if !crate_name(alias) || !crate_name(package) {
        return None;
    }
    let specifier = string_field(item, "path")
        .filter(|path| {
            !path.is_empty()
                && path.len() <= MAXIMUM_BINDING_BYTES
                && !path.starts_with('/')
                && !path.contains(['\\', '\0'])
        })
        .map_or_else(
            || "<cargo-nonlocal-dependency>".to_owned(),
            |path| {
                if path.starts_with('.') {
                    path.to_owned()
                } else {
                    format!("./{path}")
                }
            },
        );
    Some(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: specifier,
        imported_name: package.to_owned(),
        local_name: alias.replace('-', "_"),
        span,
    })
}

fn string_field<'a>(item: &'a Item, name: &str) -> Option<&'a str> {
    match item {
        Item::Table(table) => table.get(name).and_then(Item::as_str),
        Item::Value(Value::InlineTable(table)) => table.get(name).and_then(Value::as_str),
        _ => None,
    }
}

fn crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAXIMUM_BINDING_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
