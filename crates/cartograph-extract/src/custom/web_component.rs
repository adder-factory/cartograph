//! Vue and Svelte single-file components.
//!
//! Each file is one exported component named after the file. Its `<script>`
//! programs, `{{ … }}` / `{ … }` template expressions, and event-handler
//! attribute expressions run through the native JavaScript/TypeScript walker as
//! embedded regions owned by the component (see `walk::embedded_script`).
//! Markup is scanned only outside `<script>` and `<style>` raw text, HTML
//! comments, and template expressions: capitalized tags are component uses and
//! `SvelteKit` form `action` attributes name server actions.

use cartograph_domain::{
    FileParseStatus, ReferenceKind, SourceLanguage, SourceSpan, SymbolId, SymbolKind, Visibility,
};

use crate::{
    DiagnosticCode, ExtractError, ExtractedSymbol, ExtractionDiagnostic, SymbolExportFlags,
    budget::diagnostic_budget_bytes,
    walk::embedded_script::{
        EmbeddedComponent, EmbeddedFacts, EmbeddedImportSite, EmbeddedLease, EmbeddedRequest,
        SVELTE_RUNES, SVELTE_SCRIPT_POLICY, ScriptDialect, ScriptRegion, VUE_COMPILER_MACROS,
        VUE_SCRIPT_POLICY, extract_component_scripts,
    },
};

use super::{
    CustomBuilder, CustomReferenceInput, CustomSymbolInput, CustomSymbolParts, MarkupTag,
    SymbolOptions, basename_stem, bounded_string, custom_symbol, first_identifier,
    function_like_names,
};

mod layout;
mod template_scan;

use layout::{TemplateSurface, component_layout, template_expression_regions};
use template_scan::{SVELTE_DELIMITERS, TemplateDelimiters, TemplateScanner, VUE_DELIMITERS};

/// Event-handler attributes whose value is a script expression.
const EVENT_HANDLER_ATTRIBUTES: &[&str] = &["@click", "v-on:click", "on:click", "onclick"];
/// `SvelteKit` form attribute naming a server action (`action="?/create"`).
const FORM_ACTION_ATTRIBUTE: &str = "action";
/// Keywords that look like calls in a form action value.
const ACTION_SKIP_KEYWORDS: &[&str] = &["if", "for", "while", "switch", "catch", "function"];

/// The exported, default, public file-level component of a web-component file.
fn file_component_options(name: &str) -> SymbolOptions {
    SymbolOptions {
        body_search_text: format!("component {name}"),
        export: SymbolExportFlags::new(true, true),
        visibility: Some(Visibility::Public),
        ..SymbolOptions::default()
    }
}

/// File-level component symbol shared by every web-component host (Vue,
/// Svelte, and Astro): custom structural digest, default health, and a
/// `component <name>` search text.
pub(crate) fn file_component_symbol(
    id: SymbolId,
    name: &str,
    span: SourceSpan,
) -> Result<ExtractedSymbol, ExtractError> {
    custom_symbol(CustomSymbolParts {
        id,
        kind: SymbolKind::Component,
        name,
        qualified_name: bounded_string(name)?,
        span,
        options: file_component_options(name),
    })
}

pub(super) fn extract_component_file(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<FileParseStatus, ExtractError> {
    if builder.source().is_empty() {
        return Ok(FileParseStatus::Parsed);
    }
    let component = add_file_component(builder)?;
    let mut scanner = TemplateScanner::new(
        builder.source(),
        template_delimiters(builder),
        builder.cancelled,
    )?;
    let layout = component_layout(&mut scanner)?;
    let mut regions = layout.script_regions();
    let surface = TemplateSurface {
        layout: &layout,
        dialect: template_dialect(&regions),
    };
    regions.extend(template_expression_regions(&mut scanner, &surface)?);
    let template_truncated = scanner.budget_exhausted();
    let markup = ComponentMarkup {
        component: &component,
        surface: &surface,
    };
    regions.extend(scan_component_markup(builder, &markup)?);
    regions.sort_by_key(ScriptRegion::start);
    let mut facts = extract_component_regions(builder, &component, &regions)?;
    if template_truncated {
        facts.record_truncated_template();
    }
    merge_component_facts(builder, &component, facts)
}

/// Emit the file-level component, spanning the file's first line.
fn add_file_component(builder: &mut CustomBuilder<'_, '_>) -> Result<SymbolId, ExtractError> {
    let component_name = basename_stem(builder.path()).to_owned();
    let component_span_end = builder
        .source()
        .find('\n')
        .map_or(builder.source().len(), |index| index.saturating_add(1))
        .max(1);
    builder.add_symbol(
        CustomSymbolInput::new(
            SymbolKind::Component,
            &component_name,
            component_name.clone(),
        )
        .at(0, component_span_end)
        .with_options(file_component_options(&component_name)),
    )
}

/// Vue's `{{ … }}` or Svelte's `{ … }` template expression delimiters.
fn template_delimiters(builder: &CustomBuilder<'_, '_>) -> TemplateDelimiters {
    if builder.snapshot.language() == SourceLanguage::Vue {
        VUE_DELIMITERS
    } else {
        SVELTE_DELIMITERS
    }
}

/// Template expressions are TypeScript when any script region is typed.
fn template_dialect(regions: &[ScriptRegion]) -> ScriptDialect {
    if regions.iter().any(|region| region.dialect().is_typed()) {
        ScriptDialect::TypeScript
    } else {
        ScriptDialect::JavaScript
    }
}

/// The component and the template it owns.
struct ComponentMarkup<'markup, 'layout, 'source> {
    component: &'markup SymbolId,
    surface: &'markup TemplateSurface<'layout, 'source>,
}

/// Component-use references, event-handler expressions, and form actions of
/// the opening tags the layout found: never inside raw-text elements (v1
/// skipped script and style content the same way), comments, or template
/// expressions. A tag name starts right after its `<`. Returns the handler
/// expression regions.
fn scan_component_markup(
    builder: &mut CustomBuilder<'_, '_>,
    markup: &ComponentMarkup<'_, '_, '_>,
) -> Result<Vec<ScriptRegion>, ExtractError> {
    let mut regions = Vec::new();
    for &tag in &markup.surface.layout.tags {
        builder.check_cancelled()?;
        if tag
            .name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_uppercase)
        {
            builder.add_reference(
                CustomReferenceInput::new(
                    Some(markup.component.clone()),
                    tag.name,
                    ReferenceKind::References,
                )
                .at(tag.start + 1, tag.start + 1 + tag.name.len()),
            )?;
        }
        regions.extend(event_handler_regions(builder, markup, tag)?);
        scan_form_action(builder, markup.component, tag)?;
    }
    Ok(regions)
}

/// A handler value naming a handler (`@click="save"`, `on:click={save}`,
/// `@click="store.save"`) calls it; any other quoted value is a script
/// expression region. Braced Svelte expressions belong to the template
/// expression scan, except a braced handler name.
fn event_handler_regions(
    builder: &mut CustomBuilder<'_, '_>,
    markup: &ComponentMarkup<'_, '_, '_>,
    tag: MarkupTag<'_>,
) -> Result<Vec<ScriptRegion>, ExtractError> {
    let svelte = builder.snapshot.language() == SourceLanguage::Svelte;
    let mut regions = Vec::new();
    for key in EVENT_HANDLER_ATTRIBUTES {
        let mut scanner = TemplateScanner::new(tag.raw, SVELTE_DELIMITERS, builder.cancelled)?;
        let Some(attribute) = scanner.attribute(key)? else {
            continue;
        };
        let offset = tag.start + 1 + attribute.start;
        let value = attribute.value;
        let handler = if attribute.braced {
            svelte
                .then(|| handler_path(value))
                .flatten()
                .map(|(relative, name)| (offset + relative, name))
        } else if svelte && value.contains('{') {
            braced_handler(value).map(|(relative, name)| (offset + relative, name))
        } else {
            if handler_path(value).is_none() && !value.trim().is_empty() {
                regions.push(ScriptRegion::template_expression(
                    offset,
                    offset + value.len(),
                    markup.surface.dialect,
                ));
            }
            handler_path(value).map(|(relative, name)| (offset + relative, name))
        };
        if let Some((start, name)) = handler {
            builder.add_reference(
                CustomReferenceInput::new(
                    Some(markup.component.clone()),
                    name,
                    ReferenceKind::Calls,
                )
                .at(start, start + name.len()),
            )?;
        }
    }
    Ok(regions)
}

/// A handler path wrapped in one pair of braces, with its offset in `value`.
fn braced_handler(value: &str) -> Option<(usize, &str)> {
    let open = value.find('{')?;
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?;
    let (relative, handler) = handler_path(inner)?;
    Some((open + 1 + relative, handler))
}

/// A bare handler identifier or member path, with its offset in `value`.
fn handler_path(value: &str) -> Option<(usize, &str)> {
    let trimmed = value.trim();
    let path_segment = |segment: &str| {
        let mut bytes = segment.bytes();
        bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte == b'$' || byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte == b'_' || byte == b'$' || byte.is_ascii_alphanumeric())
    };
    trimmed
        .split('.')
        .all(path_segment)
        .then(|| (value.len() - value.trim_start().len(), trimmed))
}

/// A `SvelteKit` `action="?/create"` names the server action `create`. A
/// braced Svelte value is a template expression the walker already visits, so
/// its text is not scanned again (string literals inside it are not calls).
fn scan_form_action(
    builder: &mut CustomBuilder<'_, '_>,
    component: &SymbolId,
    tag: MarkupTag<'_>,
) -> Result<(), ExtractError> {
    let mut scanner = TemplateScanner::new(tag.raw, SVELTE_DELIMITERS, builder.cancelled)?;
    let Some(attribute) = scanner.attribute(FORM_ACTION_ATTRIBUTE)? else {
        return Ok(());
    };
    let offset = tag.start + 1 + attribute.start;
    let value = attribute.value;
    if super::specifier_may_carry_credential(value) {
        return Ok(());
    }
    if attribute.braced
        || (builder.snapshot.language() == SourceLanguage::Svelte && value.contains('{'))
    {
        return Ok(());
    }
    let calls = function_like_names(value);
    let names = if calls.is_empty() {
        first_identifier(value).into_iter().collect()
    } else {
        calls
    };
    for (relative, name) in names {
        if ACTION_SKIP_KEYWORDS.contains(&name)
            || VUE_COMPILER_MACROS.contains(&name)
            || SVELTE_RUNES.contains(&name)
        {
            continue;
        }
        let start = offset + relative;
        builder.add_reference(
            CustomReferenceInput::new(Some(component.clone()), name, ReferenceKind::Calls)
                .at(start, start + name.len()),
        )?;
    }
    Ok(())
}

fn extract_component_regions(
    builder: &mut CustomBuilder<'_, '_>,
    component: &SymbolId,
    regions: &[ScriptRegion],
) -> Result<EmbeddedFacts, ExtractError> {
    let policy = if builder.snapshot.language() == SourceLanguage::Vue {
        VUE_SCRIPT_POLICY
    } else {
        SVELTE_SCRIPT_POLICY
    };
    extract_component_scripts(
        EmbeddedRequest {
            component: EmbeddedComponent {
                snapshot: builder.snapshot,
                id: component,
                policy,
                maximum_ast_depth: builder.maximum_ast_depth,
            },
            regions,
            lease: EmbeddedLease {
                identities: &mut builder.identities,
                budget: &mut builder.budget,
            },
        },
        &mut *builder.cancelled,
    )
}

/// Retain the embedded facts, keep the framework virtual-module resources of
/// static imports, and report embedded regions that lost facts (script syntax
/// errors, template expressions too deep to walk, a template whose
/// unterminated structure used up the scan budget) as a partial parse.
fn merge_component_facts(
    builder: &mut CustomBuilder<'_, '_>,
    component: &SymbolId,
    mut facts: EmbeddedFacts,
) -> Result<FileParseStatus, ExtractError> {
    builder.symbols.append(&mut facts.symbols);
    builder.containments.append(&mut facts.containments);
    builder.references.append(&mut facts.references);
    builder.call_scope_sites.append(&mut facts.call_scope_sites);
    builder
        .javascript_member_calls
        .append(&mut facts.javascript_member_calls);
    builder.import_bindings.append(&mut facts.import_bindings);
    for site in &facts.import_sites {
        if framework_virtual_module(builder.snapshot.language(), &site.module) {
            add_framework_virtual_module(builder, component, site)?;
        }
    }
    let parse_status = if facts.diagnostics.is_empty() {
        FileParseStatus::Parsed
    } else {
        FileParseStatus::Partial
    };
    if facts.shortened_canonical_names {
        facts.diagnostics.push(ExtractionDiagnostic {
            code: DiagnosticCode::CanonicalNameTruncated,
            span: None,
        });
    }
    for diagnostic in facts.diagnostics {
        builder
            .budget
            .reserve_fact(diagnostic_budget_bytes(), std::iter::empty())?;
        builder.diagnostics.push(diagnostic);
    }
    Ok(parse_status)
}

fn add_framework_virtual_module(
    builder: &mut CustomBuilder<'_, '_>,
    component: &SymbolId,
    site: &EmbeddedImportSite,
) -> Result<(), ExtractError> {
    builder.add_symbol(
        CustomSymbolInput::new(
            SymbolKind::Resource,
            &site.module,
            format!(
                "{}::framework-module::{}",
                basename_stem(builder.path()),
                site.module
            ),
        )
        .at(site.start, site.end)
        .with_options(SymbolOptions {
            body_search_text: format!("framework virtual module {}", site.module),
            parent: Some(component.clone()),
            ..SymbolOptions::default()
        }),
    )?;
    Ok(())
}

fn framework_virtual_module(language: SourceLanguage, module: &str) -> bool {
    let prefixes: &[&str] = match language {
        SourceLanguage::Svelte => &[
            "$app/navigation",
            "$app/stores",
            "$app/environment",
            "$app/forms",
            "$app/paths",
            "$env/static/private",
            "$env/static/public",
            "$env/dynamic/private",
            "$env/dynamic/public",
        ],
        SourceLanguage::Vue => &["#imports", "#components", "#app", "#build", "#head"],
        _ => return false,
    };
    prefixes.iter().any(|prefix| {
        module == *prefix
            || module
                .strip_prefix(*prefix)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}
