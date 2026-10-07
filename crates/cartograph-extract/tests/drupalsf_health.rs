//! Structural release limits for the new Drupal/Salesforce modules.

mod dependency_ownership;

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_BYTES: usize = 1_024 * 1_024;
const MAX_CYCLOMATIC: u16 = 14;
const MAX_PARAMETERS: u16 = 3;
const MAX_LINES: u32 = 99;
const MAX_CALLEES: usize = 24;

#[test]
fn drupalsf_production_functions_respect_code_health_limits() {
    let limits = SourceLimits::new(SOURCE_BYTES).unwrap_or_else(|error| panic!("{error}"));
    let mut problems = Vec::new();
    for (path, source) in modules() {
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("{error}"));
        let file = NativeExtractor::new(snapshot.language())
            .and_then(|mut extractor| extractor.extract(&snapshot))
            .unwrap_or_else(|error| panic!("{error}"));
        for symbol in file
            .symbols
            .iter()
            .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method))
        {
            let metrics = symbol.health;
            let lines = symbol.span.end_line() - symbol.span.start_line() + 1;
            let callees = file
                .references
                .iter()
                .filter(|reference| {
                    reference.kind == ReferenceKind::Calls
                        && reference.owner.as_ref() == Some(&symbol.id)
                })
                .map(|reference| reference.name.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            if metrics.cyclomatic > MAX_CYCLOMATIC
                || metrics.parameter_count > MAX_PARAMETERS
                || lines > MAX_LINES
                || callees > MAX_CALLEES
                || metrics.magic_numbers != 0
                || metrics.accidental_quadratic != 0
            {
                problems.push(format!("{path}:{} {}: cc={} params={} lines={lines} callees={callees} magic={} quadratic={}",
                    symbol.span.start_line(), symbol.name, metrics.cyclomatic, metrics.parameter_count,
                    metrics.magic_numbers, metrics.accidental_quadratic));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

fn modules() -> [(&'static str, &'static str); 14] {
    [
        ("hooks.rs", include_str!("../src/framework_drupal/hooks.rs")),
        (
            "routes.rs",
            include_str!("../src/framework_drupal/routes.rs"),
        ),
        (
            "service_details.rs",
            include_str!("../src/framework_drupal/service_details.rs"),
        ),
        ("tags.rs", include_str!("../src/framework_drupal/tags.rs")),
        (
            "literal_bindings.rs",
            include_str!("../src/framework/literal_bindings.rs"),
        ),
        (
            "framework_salesforce.rs",
            include_str!("../src/framework_salesforce.rs"),
        ),
        (
            "clients.rs",
            include_str!("../src/framework_salesforce/clients.rs"),
        ),
        (
            "servers.rs",
            include_str!("../src/framework_salesforce/servers.rs"),
        ),
        (
            "salesforce_bundle.rs",
            include_str!("../src/salesforce_bundle.rs"),
        ),
        (
            "drupal_resolution.rs",
            include_str!("../../cartograph-indexer/src/native_pipeline/drupal_resolution.rs"),
        ),
        (
            "classes.rs",
            include_str!(
                "../../cartograph-indexer/src/native_pipeline/drupal_resolution/classes.rs"
            ),
        ),
        (
            "services.rs",
            include_str!(
                "../../cartograph-indexer/src/native_pipeline/drupal_resolution/services.rs"
            ),
        ),
        (
            "drupal_tags.rs",
            include_str!("../../cartograph-indexer/src/native_pipeline/drupal_tags.rs"),
        ),
        (
            "bundles.rs",
            include_str!(
                "../../cartograph-indexer/src/native_pipeline/salesforce_resolution/bundles.rs"
            ),
        ),
    ]
}
