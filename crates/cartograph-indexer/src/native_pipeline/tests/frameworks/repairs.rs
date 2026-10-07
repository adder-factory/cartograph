//! Counterexamples from the independent resolution verifier.

use super::*;

#[test]
fn explicit_apex_namespace_cannot_bind_an_unmanaged_local_class() {
    let facts = generation(&[
        ("sfdx-project.json", r#"{"namespace":""}"#),
        (
            "classes/Orders.cls",
            "public class Orders { @AuraEnabled public static void fetch() {} }",
        ),
        (
            "lwc/orders.js",
            "import fetch from '@salesforce/apex/vendor.Orders.fetch'; export function run() { fetch(); }",
        ),
        (
            "pages/Edit.page",
            r#"<apex:page controller="vendor.Orders"><apex:commandButton action="{!fetch}"/></apex:page>"#,
        ),
        (
            "aura/orders/orders.cmp",
            r#"<aura:component controller="vendor.Orders"/>"#,
        ),
    ]);
    for (name, kind) in [
        (
            "@salesforce/apex/vendor.Orders.fetch",
            ReferenceKind::Imports,
        ),
        ("fetch", ReferenceKind::Calls),
    ] {
        assert!(
            reference(&facts, ("lwc/orders.js", name, kind))
                .target_symbol_id
                .is_none()
        );
    }
    for (path, name, kind) in [
        (
            "pages/Edit.page",
            "vendor.Orders",
            ReferenceKind::References,
        ),
        ("pages/Edit.page", "fetch", ReferenceKind::Calls),
        (
            "aura/orders/orders.cmp",
            "vendor.Orders",
            ReferenceKind::References,
        ),
    ] {
        assert!(
            reference(&facts, (path, name, kind))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn visualforce_actions_do_not_prefer_an_aura_enabled_overload() {
    let facts = generation(&[
        (
            "pages/Edit.page",
            r#"<apex:page controller="EditController"><apex:commandButton action="{!save}"/></apex:page>"#,
        ),
        (
            "classes/EditController.cls",
            "public class EditController { public PageReference save() { return null; } @AuraEnabled public static String save(String value) { return value; } }",
        ),
    ]);
    assert!(
        reference(&facts, ("pages/Edit.page", "save", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn visualforce_tags_require_visualforce_component_declarations() {
    let markup = "<apex:page><c:accountCard/></apex:page>";
    let aura_only = generation(&[
        ("pages/Edit.page", markup),
        ("aura/accountCard/accountCard.cmp", "<aura:component/>"),
        ("pages/accountCard.page", "<apex:page/>"),
    ]);
    assert!(
        reference(
            &aura_only,
            ("pages/Edit.page", "AccountCard", ReferenceKind::References)
        )
        .target_symbol_id
        .is_none()
    );
    let component = generation(&[
        ("pages/Edit.page", markup),
        ("aura/accountCard/accountCard.cmp", "<aura:component/>"),
        ("components/accountCard.component", "<apex:component/>"),
    ]);
    targets(
        reference(
            &component,
            ("pages/Edit.page", "AccountCard", ReferenceKind::References),
        ),
        capability_symbol(
            &component,
            "components/accountCard.component",
            "accountCard",
        ),
        "framework-salesforce-component-convention",
    );
}

#[test]
fn cargo_dependency_renames_cannot_select_a_global_package_name() {
    for dependencies in [
        "[dependencies]\ntoolkit = { package = 'actual', path = '../actual' }\n",
        "",
        "[dependencies]\ntoolkit = { package = 'actual', path = '../actual' }\n[target.'cfg(other)'.dependencies]\ntoolkit = { path = '../toolkit' }\n",
    ] {
        let manifest = format!("[package]\nname = 'app'\nedition = '2024'\n{dependencies}");
        let facts = generation(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = ['toolkit', 'actual', 'app']\nresolver = '3'\n",
            ),
            (
                "toolkit/Cargo.toml",
                "[package]\nname = 'toolkit'\nedition = '2024'\n",
            ),
            (
                "actual/Cargo.toml",
                "[package]\nname = 'actual'\nedition = '2024'\n",
            ),
            ("app/Cargo.toml", &manifest),
            ("toolkit/src/lib.rs", "pub fn toolkit() {}"),
            ("actual/src/lib.rs", "pub fn actual() {}"),
            ("app/src/lib.rs", "use toolkit as tools;"),
        ]);
        assert!(
            reference(
                &facts,
                ("app/src/lib.rs", "tools", ReferenceKind::References)
            )
            .target_symbol_id
            .is_none()
        );
    }
}

#[test]
fn php_global_controller_routes_require_a_global_declaration() {
    let route = "show:\n  path: /show\n  controller: '\\OrderController::show'\n";
    let other = "<?php namespace Other; class OrderController { public function show() {} }";
    let php_route =
        "<?php use Other\\OrderController; Route::get('/show', '\\OrderController@show');";
    let missing = generation(&[
        ("config/routes.yaml", route),
        ("src/Other/OrderController.php", other),
        ("routes/web.php", php_route),
    ]);
    for (path, name) in [
        ("config/routes.yaml", r"\OrderController::show"),
        ("routes/web.php", r"\OrderController@show"),
    ] {
        assert!(
            reference(&missing, (path, name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
    let global = generation(&[
        ("config/routes.yaml", route),
        ("src/Other/OrderController.php", other),
        ("routes/web.php", php_route),
        (
            "src/OrderController.php",
            "<?php class OrderController { public function show() {} }",
        ),
    ]);
    for (path, name) in [
        ("config/routes.yaml", r"\OrderController::show"),
        ("routes/web.php", r"\OrderController@show"),
    ] {
        targets(
            reference(&global, (path, name, ReferenceKind::Calls)),
            capability_symbol(&global, "src/OrderController.php", "OrderController::show"),
            "framework-php-controller-qualified",
        );
    }
}

#[test]
fn php_route_imports_keep_the_existing_exact_target_and_provenance() {
    let facts = generation(&[
        (
            "routes/web.php",
            "<?php use App\\Http\\OrderController as AliasController; Route::get('/show', [AliasController::class, 'show']);",
        ),
        (
            "src/Http/OrderController.php",
            "<?php namespace App\\Http; class OrderController { public function show() {} }",
        ),
    ]);
    let call = reference(&facts, ("routes/web.php", "show", ReferenceKind::Calls));
    targets(
        call,
        capability_symbol(
            &facts,
            "src/Http/OrderController.php",
            r"App\Http::OrderController::show",
        ),
        IMPORT_BINDING_PROVENANCE,
    );
    assert_eq!(call.confidence, 1.0);
    for kind in ["function", "const"] {
        let source = format!(
            "<?php use {kind} App\\Http\\UtilityController as AliasController; Route::get('/show', [AliasController::class, 'show']);"
        );
        let non_class = generation(&[
            ("routes/web.php", &source),
            (
                "src/Http/UtilityController.php",
                "<?php namespace App\\Http; class UtilityController { public function show() {} }",
            ),
        ]);
        assert!(
            reference(&non_class, ("routes/web.php", "show", ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn rust_conventions_cannot_intercept_an_exact_included_enum() {
    let facts = generation(&[
        (
            "src/lib.rs",
            "include!(\"schema.rs\"); pub fn accept(_: Mode) {};",
        ),
        ("src/schema.rs", "pub enum Mode { Ready }"),
    ]);
    let mode = reference(&facts, ("src/lib.rs", "Mode", ReferenceKind::TypeOf));
    targets(
        mode,
        capability_symbol(&facts, "src/schema.rs", "Mode"),
        EXACT_PROJECT_PROVENANCE,
    );
    assert_eq!(mode.confidence, 0.95);
}

#[test]
fn salesforce_and_play_method_lookups_have_linear_deterministic_work() {
    let mut curves = Vec::new();
    for language in ["apex", "java"] {
        let small = method_lookup_work(language, 128);
        let large = method_lookup_work(language, 256);
        curves.push((language, small, large));
    }
    assert!(
        curves
            .iter()
            .all(|(_, small, large)| *large <= small * 2 + 256),
        "doubling lookups may add only linear work with bounded overhead: {curves:?}"
    );
}

fn method_fixtures(language: &str, count: usize) -> Vec<(String, String)> {
    (0..count).map(|ordinal| {
        if language == "apex" {
            (format!("classes/C{ordinal}.cls"), format!("public class C{ordinal} {{ @AuraEnabled public static void fetch() {{}} }}"))
        } else {
            (format!("app/controllers/C{ordinal}.java"), format!("package controllers; public class C{ordinal} {{ public String show() {{ return \"{ordinal}\"; }} }}"))
        }
    }).collect()
}

fn method_consumer(language: &str, count: usize) -> (&'static str, String) {
    let source = (0..count)
        .map(|ordinal| {
            if language == "apex" {
                format!("import f{ordinal} from '@salesforce/apex/C{ordinal}.fetch';\n")
            } else {
                format!("GET /c{ordinal} controllers.C{ordinal}.show()\n")
            }
        })
        .collect();
    (
        if language == "apex" {
            "lwc/consumer.js"
        } else {
            "conf/routes"
        },
        source,
    )
}

fn method_lookup_work(language: &str, count: usize) -> u64 {
    let mut fixtures = method_fixtures(language, count);
    let (consumer, source) = method_consumer(language, count);
    fixtures.push((consumer.to_owned(), String::new()));
    let baseline = fixtures
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let (result, baseline_polls) = counted_resolution(&baseline, None);
    assert!(result.is_ok());
    let entry = fixtures
        .last_mut()
        .unwrap_or_else(|| panic!("consumer missing"));
    entry.1 = source;
    let inputs = fixtures
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let (result, polls) = counted_resolution(&inputs, None);
    let (facts, _) = result.unwrap_or_else(|_| panic!("method resolution failed"));
    assert_method_targets(&facts, language, count);
    let stop = polls.saturating_sub(8);
    let (cancelled, actual) = counted_resolution(&inputs, Some(stop));
    assert_matches!(cancelled, Err(ResolveGenerationFailure { reason: None }));
    assert_eq!(actual, stop);
    polls
        .checked_sub(baseline_polls)
        .unwrap_or_else(|| panic!("invalid work baseline"))
}

fn assert_method_targets(facts: &GenerationFacts, language: &str, count: usize) {
    let symbols = facts
        .symbols
        .iter()
        .map(|symbol| (&symbol.symbol_id, symbol))
        .collect::<HashMap<_, _>>();
    let references = facts
        .references
        .iter()
        .filter(|reference| {
            reference.reference_name.starts_with("@salesforce/apex/C")
                || reference.reference_name.starts_with("controllers.C")
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), count);
    for site in references {
        let name = site
            .reference_name
            .strip_prefix("@salesforce/apex/")
            .unwrap_or(&site.reference_name)
            .replace('.', "::");
        let target = site
            .target_symbol_id
            .as_ref()
            .and_then(|id| symbols.get(id))
            .unwrap_or_else(|| panic!("unresolved method: {site:?}"));
        targets(
            site,
            target,
            if language == "apex" {
                "framework-salesforce-apex-method"
            } else {
                "framework-play-qualified-handler"
            },
        );
        assert_eq!(target.qualified_name, name);
    }
}
