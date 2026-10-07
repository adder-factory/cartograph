//! Each bridge proves target identity, provenance and an abstention boundary.

use super::*;
use std::fmt::Write;

#[test]
fn drupal_form_routes_use_the_full_php_namespace() {
    let facts = generation(&[
        (
            "modules/demo/demo.routing.yml",
            "demo:\n  path: '/settings'\n  defaults:\n    _form: '\\Drupal\\demo\\Form\\SettingsForm'\n  methods: [GET, POST]\nmissing:\n  path: '/missing'\n  defaults:\n    _form: '\\Missing\\SettingsForm'\n",
        ),
        (
            "modules/demo/src/Form/SettingsForm.php",
            "<?php namespace Drupal\\demo\\Form; class SettingsForm {}",
        ),
        (
            "other/SettingsForm.php",
            "<?php namespace Other; class SettingsForm {}",
        ),
    ]);
    targets(
        reference(
            &facts,
            (
                "modules/demo/demo.routing.yml",
                "\\Drupal\\demo\\Form\\SettingsForm",
                ReferenceKind::References,
            ),
        ),
        capability_symbol(
            &facts,
            "modules/demo/src/Form/SettingsForm.php",
            "Drupal\\demo\\Form::SettingsForm",
        ),
        EXACT_PROJECT_PROVENANCE,
    );
    assert!(
        reference(
            &facts,
            (
                "modules/demo/demo.routing.yml",
                "\\Missing\\SettingsForm",
                ReferenceKind::References
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn drupal_service_classes_ids_aliases_parents_and_factories_resolve() {
    let path = "modules/demo/demo.services.yml";
    let facts = generation(&[
        (
            path,
            "services:\n  demo.base:\n    class: Drupal\\demo\\Manager\n  demo.alias:\n    alias: demo.base\n  demo.child:\n    parent: demo.base\n    factory: '@demo.base:create'\n  demo.missing:\n    class: Missing\\Manager\n  Drupal\\demo\\Shorthand: ~\n",
        ),
        (
            "modules/demo/src/Services.php",
            "<?php namespace Drupal\\demo; class Manager {} class Shorthand {}",
        ),
        (
            "other/Manager.php",
            "<?php namespace Other; class Manager {}",
        ),
    ]);
    targets(
        reference(
            &facts,
            (path, "Drupal\\demo\\Manager", ReferenceKind::References),
        ),
        capability_symbol(
            &facts,
            "modules/demo/src/Services.php",
            "Drupal\\demo::Manager",
        ),
        EXACT_PROJECT_PROVENANCE,
    );
    targets(
        reference(
            &facts,
            (path, "Drupal\\demo\\Shorthand", ReferenceKind::References),
        ),
        capability_symbol(
            &facts,
            "modules/demo/src/Services.php",
            "Drupal\\demo::Shorthand",
        ),
        EXACT_PROJECT_PROVENANCE,
    );
    let service = capability_symbol(&facts, path, &format!("{path}::drupal-service::demo.base"));
    let refs = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "demo.base")
        .collect::<Vec<_>>();
    assert_eq!(refs.len(), 3);
    for reference in refs {
        targets(reference, service, "framework-drupal-resource");
    }
    assert!(
        reference(
            &facts,
            (path, "Missing\\Manager", ReferenceKind::References)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn drupal_flow_and_sequence_factory_classes_do_not_use_same_name_decoys() {
    let path = "modules/demo/demo.services.yml";
    let facts = generation(&[
        (
            path,
            "services:\n  demo.flow: { class: Drupal\\demo\\Helper }\n  demo.made:\n    factory: ['Drupal\\demo\\Factory', 'create']\n  demo.missing:\n    factory: ['Missing\\Factory', 'create']\n",
        ),
        (
            "modules/demo/src/Services.php",
            "<?php namespace Drupal\\demo; class Helper {} class Factory {}",
        ),
        (
            "other/Services.php",
            "<?php namespace Other; class Helper {} class Factory {}",
        ),
    ]);
    for name in ["Helper", "Factory"] {
        targets(
            reference(
                &facts,
                (
                    path,
                    &format!("Drupal\\demo\\{name}"),
                    ReferenceKind::References,
                ),
            ),
            capability_symbol(
                &facts,
                "modules/demo/src/Services.php",
                &format!("Drupal\\demo::{name}"),
            ),
            EXACT_PROJECT_PROVENANCE,
        );
    }
    assert!(
        reference(
            &facts,
            (path, "Missing\\Factory", ReferenceKind::References)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn drupal_nested_argument_class_and_factory_data_do_not_bind_php_classes() {
    let path = "modules/demo/demo.services.yml";
    let facts = generation(&[
        (
            path,
            "services:\n  demo:\n    class: Vendor\\Service\n    arguments:\n      class: Vendor\\Data\n      factory: Vendor\\Data::make\n      sequence:\n        factory: ['Vendor\\Data', 'make']\n  demo.flow: { class: Vendor\\Service, arguments: { class: Vendor\\Data } }\n",
        ),
        (
            "src/Services.php",
            "<?php namespace Vendor; class Service {} class Data { public static function make() {} }",
        ),
    ]);
    let service = capability_symbol(&facts, "src/Services.php", "Vendor::Service");
    let class_refs = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "Vendor\\Service")
        .collect::<Vec<_>>();
    assert_eq!(class_refs.len(), 2);
    for reference in class_refs {
        targets(reference, service, EXACT_PROJECT_PROVENANCE);
    }
    let data = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "Vendor\\Data")
        .collect::<Vec<_>>();
    assert_eq!(data.len(), 2);
    assert!(
        data.iter()
            .all(|reference| reference.target_symbol_id.is_none())
    );
}

#[test]
fn drupal_fqcn_service_aliases_preserve_service_identity() {
    let path = "modules/demo/demo.services.yml";
    let facts = generation(&[
        (
            path,
            "services:\n  Vendor\\Thing: ~\n  demo.alias:\n    alias: Vendor\\Thing\n  demo.parent:\n    parent: Vendor\\Thing\n",
        ),
        ("src/Thing.php", "<?php namespace Vendor; class Thing {}"),
    ]);
    let class = capability_symbol(&facts, "src/Thing.php", "Vendor::Thing");
    let service = capability_symbol(
        &facts,
        path,
        &format!("{path}::drupal-service::Vendor\\Thing"),
    );
    let uses = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "Vendor\\Thing")
        .collect::<Vec<_>>();
    assert_eq!(uses.len(), 3);
    for reference in uses {
        if reference.owner_symbol_id.as_ref() == Some(&service.symbol_id) {
            targets(reference, class, EXACT_PROJECT_PROVENANCE);
        } else {
            targets(reference, service, "framework-drupal-resource");
        }
    }
    assert_eq!(
        facts
            .references()
            .iter()
            .filter(|reference| reference.target_symbol_id.as_ref() == Some(&service.symbol_id))
            .count(),
        2
    );
}

#[test]
fn ambiguous_drupal_framework_lookup_preserves_base_service_resolution() {
    let path = "modules/demo/demo.services.yml";
    let facts = generation(&[
        (
            path,
            "services:\n  demo.alias:\n    alias: duplicated\n  demo.class:\n    class: Vendor\\Thing\n  duplicated: ~\n",
        ),
        (
            "modules/other/other.services.yml",
            "services:\n  duplicated: ~\n",
        ),
        ("src/Thing.php", "<?php namespace Vendor; class Thing {}"),
        ("other/Thing.php", "<?php namespace Vendor; class Thing {}"),
    ]);
    let fallback = reference(&facts, (path, "duplicated", ReferenceKind::References));
    let target = facts
        .symbols()
        .iter()
        .find(|symbol| Some(&symbol.symbol_id) == fallback.target_symbol_id.as_ref())
        .unwrap_or_else(|| panic!("no base target"));
    assert_eq!(
        target.qualified_name, "modules/other/other.services.yml::drupal-service::duplicated",
        "{target:?}"
    );
    assert_eq!(fallback.resolution_provenance, EXACT_PROJECT_PROVENANCE);
    let class = reference(&facts, (path, "Vendor\\Thing", ReferenceKind::References));
    assert!(class.target_symbol_id.is_none(), "{class:?}");
}

#[test]
fn drupal_hooks_point_from_implementation_to_shared_resource() {
    let path = "modules/demo/demo.module";
    let facts = generation(&[(
        path,
        "<?php\nfunction demo_help() {}\nfunction other_help() {}\n",
    )]);
    let implementation = capability_symbol(&facts, path, "demo_help");
    let contract = capability_symbol(&facts, path, &format!("{path}::drupal-hook:hook_help"));
    let reference = reference(&facts, (path, "hook_help", ReferenceKind::References));
    targets(reference, contract, "framework-drupal-resource");
    assert_eq!(
        reference.owner_symbol_id.as_ref(),
        Some(&implementation.symbol_id)
    );
    assert!(facts.edges().iter().all(|edge| edge.source_symbol_id != contract.symbol_id || edge.kind != EdgeKind::Calls));
}

#[test]
fn drupal_tag_providers_and_consumers_share_an_outgoing_hub_per_installation() {
    let facts = generation(&[
        (
            "web/modules/a/a.services.yml",
            "services:\n  a.provider:\n    tags:\n      - { name: demo.handlers }\n",
        ),
        (
            "web/modules/b/b.services.yml",
            "services:\n  b.consumer:\n    arguments:\n      - !tagged_iterator demo.handlers\n",
        ),
        (
            "other/modules/a/a.services.yml",
            "services:\n  foreign.provider:\n    tags:\n      - { name: demo.handlers }\n",
        ),
    ]);
    let hub = capability_symbol(
        &facts,
        "web/modules/a/a.services.yml",
        "web/modules/a/a.services.yml::service-tag:demo.handlers",
    );
    for (path, service, provenance) in [
        (
            "web/modules/a/a.services.yml",
            "a.provider",
            DRUPAL_TAG_PROVIDES_PROVENANCE,
        ),
        (
            "web/modules/b/b.services.yml",
            "b.consumer",
            DRUPAL_TAG_CONSUMES_PROVENANCE,
        ),
    ] {
        let source = capability_symbol(&facts, path, &format!("{path}::drupal-service::{service}"));
        assert!(
            facts
                .edges()
                .iter()
                .any(|edge| edge.source_symbol_id == source.symbol_id
                    && edge.target_symbol_id == hub.symbol_id
                    && edge.provenance == provenance)
        );
    }
    let foreign = capability_symbol(
        &facts,
        "other/modules/a/a.services.yml",
        "other/modules/a/a.services.yml::drupal-service::foreign.provider",
    );
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.source_symbol_id != foreign.symbol_id
                || edge.target_symbol_id != hub.symbol_id)
    );
}

#[test]
fn aura_client_actions_remain_in_their_bundle_and_server_actions_use_controller_context() {
    let markup = "aura/Orders/Orders.cmp";
    let script = "aura/Orders/OrdersController.js";
    let facts = generation(&[
        (
            markup,
            "<aura:component controller=\"OrderController\"><aura:handler action=\"{!c.init}\"/><aura:handler action=\"{!c.missing}\"/></aura:component>",
        ),
        (
            script,
            "({ init: function(component, event, helper) { component.get('c.fetch'); helper.load(); this.init(); } })",
        ),
        (
            "aura/Orders/OrdersHelper.js",
            "({ load: function(component) {} })",
        ),
        (
            "aura/Other/OtherController.js",
            "({ missing: function(component) {}, init: function(component) {} })",
        ),
        (
            "classes/OrderController.cls",
            "public class OrderController { @AuraEnabled public static void fetch() {} }",
        ),
        (
            "classes/OtherController.cls",
            "public class OtherController { @AuraEnabled public static void fetch() {} }",
        ),
    ]);
    let client = capability_symbol(&facts, script, &format!("{script}::init"));
    targets(
        reference(&facts, (markup, "init", ReferenceKind::Calls)),
        client,
        "framework-salesforce-client-action",
    );
    targets(
        reference(&facts, (script, "c.fetch", ReferenceKind::Calls)),
        capability_symbol(
            &facts,
            "classes/OrderController.cls",
            "OrderController::fetch",
        ),
        "framework-salesforce-server-action",
    );
    targets(
        reference(&facts, (script, "helper.load", ReferenceKind::Calls)),
        capability_symbol(
            &facts,
            "aura/Orders/OrdersHelper.js",
            "aura/Orders/OrdersHelper.js::load",
        ),
        "framework-salesforce-client-action",
    );
    assert!(
        reference(&facts, (markup, "missing", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn aura_server_actions_preserve_namespaces_and_prefer_unique_aura_enabled_methods() {
    let script = "aura/Orders/OrdersController.js";
    let facts = generation(&[
        (
            "aura/Orders/Orders.cmp",
            "<aura:component controller=\"OrderController\"/>",
        ),
        (
            script,
            "({ init: function(cmp) { cmp.get('c.fetch'); cmp.get('c.missing'); } })",
        ),
        (
            "classes/OrderController.cls",
            "public class OrderController {\n public static void fetch(String value) {}\n @AuraEnabled public static void fetch() {}\n}",
        ),
    ]);
    let resolved = reference(&facts, (script, "c.fetch", ReferenceKind::Calls));
    assert_eq!(
        resolved.resolution_provenance,
        "framework-salesforce-server-action"
    );
    let target = facts
        .symbols()
        .iter()
        .find(|symbol| Some(&symbol.symbol_id) == resolved.target_symbol_id.as_ref())
        .unwrap_or_else(|| panic!("missing target"));
    assert_eq!(target.qualified_name, "OrderController::fetch");
    assert_eq!(target.start_line, 3);
    assert!(
        reference(&facts, (script, "c.missing", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
    let namespaced = generation(&[
        (
            "aura/Orders/Orders.cmp",
            "<aura:component controller=\"vendor.OrderController\"/>",
        ),
        (script, "({ init: function(cmp) { cmp.get('c.fetch'); } })"),
        (
            "classes/OrderController.cls",
            "public class OrderController { @AuraEnabled public static void fetch() {} }",
        ),
    ]);
    assert!(
        reference(&namespaced, (script, "c.fetch", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn lwc_component_templates_and_imports_bind_only_to_peer_bundles() {
    let facts = generation(&[
        (
            "force-app/lwc/orderCard/orderCard.js",
            "import { LightningElement } from 'lwc'; export default class OrderCard extends LightningElement {}",
        ),
        (
            "force-app/lwc/orderList/orderList.js",
            "import Card from 'c/orderCard'; export default class OrderList {}",
        ),
        (
            "force-app/lwc/orderList/orderList.html",
            "<template><c-order-card/><c-missing/></template>",
        ),
        (
            "other/lwc/orderCard/orderCard.js",
            "export default class OrderCard {}",
        ),
        (
            "other/lwc/missing/missing.js",
            "export default class Missing {}",
        ),
    ]);
    let target = capability_symbol(
        &facts,
        "force-app/lwc/orderCard/orderCard.js",
        "force-app/lwc/orderCard/orderCard.js::orderCard",
    );
    for path in [
        "force-app/lwc/orderList/orderList.js",
        "force-app/lwc/orderList/orderList.html",
    ] {
        let kind = if path.ends_with("js") {
            ReferenceKind::Imports
        } else {
            ReferenceKind::References
        };
        targets(
            reference(&facts, (path, "c/orderCard", kind)),
            target,
            "framework-salesforce-lwc-component",
        );
    }
    assert!(
        reference(
            &facts,
            (
                "force-app/lwc/orderList/orderList.html",
                "c/missing",
                ReferenceKind::References
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn duplicate_aura_actions_and_lwc_script_paths_abstain() {
    let facts = generation(&[
        (
            "aura/Orders/Orders.cmp",
            "<aura:component><aura:handler action=\"{!c.init}\"/></aura:component>",
        ),
        (
            "aura/Orders/OrdersController.js",
            "({ init: function(cmp) {}, init: function(cmp) {} })",
        ),
        ("lwc/list/list.html", "<template><c-card/></template>"),
        ("lwc/card/card.js", "export default class Card {}"),
        ("lwc/card/card.ts", "export default class Card {}"),
    ]);
    assert!(
        reference(
            &facts,
            ("aura/Orders/Orders.cmp", "init", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
    assert!(
        reference(
            &facts,
            ("lwc/list/list.html", "c/card", ReferenceKind::References)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn aura_overwritten_action_values_and_dynamic_keys_do_not_resolve_markup() {
    for source in [
        "({ init: function(cmp) {}, init: null })",
        "({ init: null, init: function(cmp) {} })",
        "({ init: function(cmp) {}, 'init': null })",
        "({ init: function(cmp) {}, '\\x69nit': null })",
        "({ init: function(cmp) {}, ['init']: null })",
        "({ init: function(cmp) {}, ...other })",
        "({ ...other, init: function(cmp) {} })",
        "({ init: function(cmp) {}, init })",
    ] {
        let facts = generation(&[
            (
                "aura/Orders/Orders.cmp",
                "<aura:component><aura:handler action=\"{!c.init}\"/></aura:component>",
            ),
            ("aura/Orders/OrdersController.js", source),
            (
                "aura/Other/OtherController.js",
                "({ init: function(cmp) {} })",
            ),
        ]);
        assert!(
            reference(
                &facts,
                ("aura/Orders/Orders.cmp", "init", ReferenceKind::Calls)
            )
            .target_symbol_id
            .is_none(),
            "{source}"
        );
    }
}

#[test]
fn aura_reassigned_shadowed_and_unbound_receivers_cannot_bind_server_actions() {
    let path = "aura/Orders/OrdersController.js";
    let facts = generation(&[
        (
            "aura/Orders/Orders.cmp",
            "<aura:component controller=\"OrderController\"/>",
        ),
        (
            path,
            "({ good: function(component, event, helper) { helper.load(component); component.get('c.fetch'); }, assigned: function(cmp) { cmp = other; cmp.get('c.fetch'); }, nested: function(component) { function local() { var cmp = other; cmp.get('c.fetch'); } }, shadowed: function(cmp) { function local(cmp) { cmp.get('c.fetch'); } }, unbound: function() { cmp.get('c.fetch'); }, pattern: function(cmp) { ({ cmp } = other); cmp.get('c.fetch'); }, dynamic: function(cmp) { eval(code); cmp.get('c.fetch'); }, wrapped: function(cmp, event, helper) { (eval)(code); cmp.get('c.fetch'); helper.load(); } })",
        ),
        (
            "aura/Orders/OrdersHelper.js",
            "({ load: function(cmp) {} })",
        ),
        (
            "classes/OrderController.cls",
            "public class OrderController { @AuraEnabled public static void fetch() {} }",
        ),
    ]);
    let server = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "c.fetch")
        .collect::<Vec<_>>();
    assert_eq!(server.len(), 1);
    targets(
        server[0],
        capability_symbol(
            &facts,
            "classes/OrderController.cls",
            "OrderController::fetch",
        ),
        "framework-salesforce-server-action",
    );
    assert!(
        facts
            .references()
            .iter()
            .any(|reference| reference.reference_name == "cmp.get")
    );
    let helper = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "helper.load")
        .collect::<Vec<_>>();
    assert_eq!(helper.len(), 2);
    let resolved = helper
        .iter()
        .copied()
        .find(|reference| reference.target_symbol_id.is_some())
        .unwrap_or_else(|| panic!("missing stable helper target"));
    targets(
        resolved,
        capability_symbol(
            &facts,
            "aura/Orders/OrdersHelper.js",
            "aura/Orders/OrdersHelper.js::load",
        ),
        "framework-salesforce-client-action",
    );
    assert_eq!(
        helper
            .iter()
            .filter(|reference| reference.target_symbol_id.is_none())
            .count(),
        1
    );
}

#[test]
fn aura_shadowed_helpers_and_nested_this_do_not_use_the_client_convention() {
    let path = "aura/Orders/OrdersController.js";
    let facts = generation(&[
        (
            path,
            "({ init: function(cmp, event, helper) { helper = other; helper.load(); function nested() { this.init(); } }, unbound: function(cmp) { helper.other(); } })",
        ),
        (
            "aura/Orders/OrdersHelper.js",
            "({ load: function(cmp) {}, other: function(cmp) {} })",
        ),
    ]);
    for name in ["helper.load", "helper.other", "this.init"] {
        let resolved = reference(&facts, (path, name, ReferenceKind::Calls));
        assert!(resolved.target_symbol_id.is_none(), "{resolved:?}");
    }
}

#[test]
fn aura_conflicting_controller_contexts_cannot_choose_an_apex_class() {
    let path = "aura/Orders/OrdersController.js";
    let facts = generation(&[
        (path, "({ init: function(cmp) { cmp.get('c.fetch'); } })"),
        (
            "aura/Orders/Orders.cmp",
            "<aura:component controller=\"Orders\"/>",
        ),
        (
            "aura/Orders/Orders.app",
            "<aura:application controller=\"Other\"/>",
        ),
        (
            "classes/Orders.cls",
            "public class Orders { @AuraEnabled public static void fetch() {} }",
        ),
        (
            "classes/Other.cls",
            "public class Other { @AuraEnabled public static void fetch() {} }",
        ),
    ]);
    assert!(
        reference(&facts, (path, "c.fetch", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn drupal_repeated_class_and_tag_lookups_are_linear_and_cancellable() {
    const SMALL_FILES: usize = 16;
    const LARGE_FILES: usize = SMALL_FILES * 2;
    const POLL_OVERHEAD: u64 = 256;
    let small = drupal_lookup_work(SMALL_FILES);
    let large = drupal_lookup_work(LARGE_FILES);
    assert!(
        large <= small * 2 + POLL_OVERHEAD,
        "lookup work {small} -> {large}"
    );
}

fn drupal_lookup_work(count: usize) -> u64 {
    const SERVICES_PER_FILE: usize = 8;
    const CANCELLATION_TAIL_POLLS: u64 = 8;
    let mut sources = Vec::new();
    for file in 0..count {
        let mut services = String::from("services:\n");
        for service in 0..SERVICES_PER_FILE {
            write!(services, "  demo.s{file}_{service}:\n    class: Vendor\\Manager\n    tags:\n      - {{ name: demo.handlers }}\n").unwrap_or_else(|error| panic!("{error}"));
        }
        sources.push((
            format!("web/modules/demo{file}/demo.services.yml"),
            services,
        ));
    }
    sources.push((
        "src/Manager.php".to_owned(),
        "<?php namespace Vendor; class Manager {}".to_owned(),
    ));
    let fixtures = sources
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let (result, polls) = counted_resolution(&fixtures, None);
    let (facts, _) = result.unwrap_or_else(|_| panic!("resolution failed"));
    assert_eq!(
        facts
            .references
            .iter()
            .filter(|reference| reference.reference_name == "Vendor\\Manager"
                && reference.target_symbol_id.is_some()
                && reference.resolution_provenance == EXACT_PROJECT_PROVENANCE)
            .count(),
        count * SERVICES_PER_FILE
    );
    let stop = polls.saturating_sub(CANCELLATION_TAIL_POLLS);
    let (cancelled, actual) = counted_resolution(&fixtures, Some(stop));
    assert!(cancelled.is_err());
    assert_eq!(actual, stop);
    polls
}
