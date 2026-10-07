//! Drupal and Salesforce framework facts through the native extractor.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_BYTES: usize = 1_024 * 1_024;

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_BYTES).unwrap_or_else(|error| panic!("{error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("{error}"));
    NativeExtractor::new(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot))
        .unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn drupal_routing_retains_all_methods_handlers_and_route_owners() {
    let source = "demo.form:\n  path: '/settings'\n  defaults:\n    _form: '\\Drupal\\demo\\Form\\SettingsForm'\n  methods: [GET, POST]\ndemo.entity:\n  path: '/entity'\n  defaults:\n    _entity_form: 'thing.add'\n    _entity_list: 'thing'\n    _entity_view: 'thing.full'\n";
    let file = extract("modules/demo/demo.routing.yml", source);
    let routes = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .collect::<Vec<_>>();
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].name, "/settings [GET,POST]");
    assert_eq!(routes[0].span.start_line(), 1);
    for name in [
        "\\Drupal\\demo\\Form\\SettingsForm",
        "thing.add",
        "thing",
        "thing.full",
    ] {
        let reference = file
            .references
            .iter()
            .find(|reference| reference.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(reference.kind, ReferenceKind::References);
        assert!(
            routes
                .iter()
                .any(|route| reference.owner.as_ref() == Some(&route.id))
        );
    }
    let unrelated = extract("config/plain.yml", source);
    assert!(
        unrelated
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Route)
    );
    let missing = extract(
        "demo.routing.yml",
        "demo:\n  defaults:\n    _form: 'SettingsForm'\n",
    );
    assert!(
        missing
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Route)
    );
}

#[test]
fn service_flow_factories_and_shorthand_record_complete_literals() {
    let source = "services:\n  demo.flow: { class: Drupal\\demo\\Helper, factory: ['Drupal\\demo\\Factory', 'create'] }\n  Drupal\\demo\\Shorthand: ~\n  demo.scalar:\n    factory: '@demo.factory:create'\n  demo.sequence:\n    factory: ['@demo.factory', 'create']\n  demo.class:\n    factory: 'Drupal\\demo\\Factory::create'\n";
    let file = extract("modules/demo/demo.services.yml", source);
    for name in [
        "Drupal\\demo\\Helper",
        "Drupal\\demo\\Factory",
        "Drupal\\demo\\Shorthand",
        "demo.factory",
    ] {
        assert!(
            file.references
                .iter()
                .any(|reference| reference.name == name
                    && reference.kind == ReferenceKind::References),
            "missing {name}"
        );
    }
    assert!(
        file.references
            .iter()
            .all(|reference| reference.name != "demo.factory:create")
    );
    let rejected = extract(
        "demo.services.yml",
        "services:\n  demo:\n    factory: ['Drupal\\demo\\Factory', dynamic(), 'extra']\n  malformed: { class: 'Unterminated }\n  misleading:\n    factory: ['Drupal\\demo\\Factory' junk, 'create']\n",
    );
    assert!(
        rejected
            .references
            .iter()
            .all(|reference| reference.name != "Drupal\\demo\\Factory"
                && reference.name != "Unterminated")
    );
}

#[test]
fn nested_service_data_does_not_acquire_php_class_intent() {
    let file = extract(
        "demo.services.yml",
        "services:\n  demo:\n    class: Vendor\\Service\n    arguments:\n      class: Vendor\\Data\n      factory: Vendor\\Data::make\n      sequence:\n        factory: ['Vendor\\Data', 'make']\n  demo.flow: { class: Vendor\\Service, arguments: { class: Vendor\\Data } }\n",
    );
    let class_sites = file
        .import_bindings
        .iter()
        .filter(|binding| binding.module_specifier == cartograph_extract::DRUPAL_CLASS_MODULE)
        .collect::<Vec<_>>();
    assert_eq!(class_sites.len(), 2);
    assert!(
        class_sites
            .iter()
            .all(|binding| binding.imported_name == "Vendor\\Service")
    );
    let data = file
        .references
        .iter()
        .filter(|reference| reference.name == "Vendor\\Data")
        .collect::<Vec<_>>();
    assert_eq!(data.len(), 2);
    assert!(
        data.iter()
            .all(|reference| reference.resolution_name.is_none())
    );
}

#[test]
fn routing_scopes_handlers_to_defaults_and_masks_comments() {
    let file = extract(
        "demo.routing.yml",
        "# ignored:\ndemo:\n  defaults: { _form: '\\Drupal\\demo\\Form' }\n  path: '/form'\n  methods: [GET, POST] # keep both\n  requirements:\n    _form: '\\Wrong\\Form'\n  options:\n    path: '/wrong'\n",
    );
    let routes = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .collect::<Vec<_>>();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].name, "/form [GET,POST]");
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "\\Drupal\\demo\\Form")
    );
    assert!(
        file.references
            .iter()
            .all(|reference| reference.name != "\\Wrong\\Form")
    );
    let duplicate = extract(
        "demo.routing.yml",
        "demo:\n  path: '/x'\n  defaults:\n    _form: 'First'\n    _form: 'Second'\n",
    );
    assert!(
        duplicate
            .references
            .iter()
            .all(|reference| reference.name != "First" && reference.name != "Second")
    );
}

#[test]
fn hook_implementations_reference_one_contract_per_hook() {
    let source = "<?php\nfunction demo_help() {}\n/** @implements hook_help() */\nfunction demo_other() {}\nfunction foreign_help() {}\n";
    let file = extract("modules/demo/demo.module", source);
    let contract = file
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "hook_help" && symbol.kind == SymbolKind::Resource)
        .collect::<Vec<_>>();
    assert_eq!(contract.len(), 1);
    assert_eq!(contract[0].span.start_line(), 1);
    assert_eq!(
        file.references
            .iter()
            .filter(|reference| reference.resolution_name.as_deref()
                == Some(&contract[0].qualified_name))
            .count(),
        2
    );
    let ordinary = extract("src/demo.php", source);
    assert!(
        ordinary
            .symbols
            .iter()
            .all(|symbol| !symbol.qualified_name.contains("::drupal-hook:"))
    );
}

#[test]
fn aura_actions_are_top_level_function_pairs_and_server_literals() {
    let source = "({\n doInit: function(component) { component.get('c.fetch'); component.get('v.rows'); component.get('c.' + name); },\n nested: { fake: function() {} },\n // ignored: function() {}\n save: function(cmp) { cmp.get(\"c.save\"); unrelated.get('c.fake'); }\n});\n";
    let file = extract("aura/Orders/OrdersController.js", source);
    let actions = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(actions, ["doInit", "save"]);
    for name in ["c.fetch", "c.save"] {
        assert!(
            file.references
                .iter()
                .any(|reference| reference.name == name
                    && reference.kind == ReferenceKind::Calls
                    && reference.owner.is_none())
        );
    }
    for name in ["v.rows", "c.fake"] {
        assert!(
            file.references
                .iter()
                .all(|reference| reference.name != name)
        );
    }
    let ordinary = extract("src/OrdersController.js", source);
    assert!(
        ordinary
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Method)
    );
    assert!(
        ordinary
            .references
            .iter()
            .all(|reference| reference.name != "c.fetch")
    );
}

#[test]
fn overwritten_and_dynamic_aura_actions_do_not_publish_methods() {
    for source in [
        "({ init: function(cmp) {}, init: null })",
        "({ init: null, init: function(cmp) {} })",
        "({ init: function(cmp) {}, 'init': null })",
        "({ init: function(cmp) {}, ['init']: null })",
        "({ init: function(cmp) {}, ...other })",
        "({ init: function(cmp) {}, init })",
    ] {
        let file = extract("aura/Orders/OrdersController.js", source);
        assert!(
            file.symbols
                .iter()
                .all(|symbol| symbol.kind != SymbolKind::Method),
            "{source}"
        );
    }
}

#[test]
fn aura_server_intent_requires_a_stable_action_component_parameter() {
    let file = extract(
        "aura/Orders/OrdersController.js",
        "({ good: function(component, event, helper) { helper.load(component); component.get('c.fetch'); }, assigned: function(cmp) { cmp = other; cmp.get('c.fetch'); }, nested: function(component) { function local() { var cmp = other; cmp.get('c.fetch'); } }, shadowed: function(cmp) { function local(cmp) { cmp.get('c.fetch'); } }, unbound: function() { cmp.get('c.fetch'); }, pattern: function(cmp) { ({ cmp } = other); cmp.get('c.fetch'); }, dynamic: function(cmp) { eval(code); cmp.get('c.fetch'); }, wrapped: function(cmp, event, helper) { (eval)(code); cmp.get('c.fetch'); helper.load(); } })",
    );
    let server = file
        .references
        .iter()
        .filter(|reference| reference.name == "c.fetch")
        .collect::<Vec<_>>();
    assert_eq!(server.len(), 1);
    assert_eq!(server[0].kind, ReferenceKind::Calls);
    assert_eq!(
        server[0].resolution_name.as_deref(),
        Some("cartograph.salesforce-controller::fetch")
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "cmp.get")
    );
    assert_eq!(
        file.import_bindings
            .iter()
            .filter(|binding| binding.module_specifier
                == cartograph_extract::SALESFORCE_CLIENT_MODULE
                && binding.imported_name == "helper.load")
            .count(),
        1
    );
}

#[test]
fn lwc_components_and_template_uses_are_distinct_facts() {
    let script = extract(
        "lwc/orderCard/orderCard.js",
        "import { LightningElement } from 'lwc';\n\nexport default class OrderCard extends LightningElement {}\n",
    );
    let component = script
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Component)
        .unwrap_or_else(|| panic!("missing component"));
    assert_eq!(
        component.qualified_name,
        "lwc/orderCard/orderCard.js::orderCard"
    );
    assert_eq!(component.span.start_line(), 3);
    let template = extract(
        "lwc/orderList/orderList.html",
        "<template><!-- <c-fake/> --><c-order-card></c-order-card></template>",
    );
    assert!(
        template
            .references
            .iter()
            .any(|reference| reference.name == "c/orderCard"
                && reference.kind == ReferenceKind::References)
    );
    assert!(
        template
            .references
            .iter()
            .all(|reference| reference.name != "c/fake")
    );
    assert!(
        template
            .symbols
            .iter()
            .all(|symbol| symbol.name != "c-order-card")
    );
    let mismatch = extract("lwc/orderCard/other.js", "export default class Other {}\n");
    assert!(
        mismatch
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Component)
    );
}
