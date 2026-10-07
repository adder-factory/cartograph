//! Verifier counterexamples retain base behavior and bound the new conventions.

use super::*;

#[test]
fn helper_parameters_are_injected_only_into_controller_entry_points() {
    let controller = "aura/Orders/OrdersController.js";
    let helper = "aura/Orders/OrdersHelper.js";
    let renderer = "aura/Orders/OrdersRenderer.js";
    let facts = generation(&[
        (
            controller,
            "({ init: function(cmp, event, helper) { cmp.get('c.fetch'); helper.invoke(cmp, event, { load: function() {} }); } })",
        ),
        (
            helper,
            "({ invoke: function(cmp, event, helper) { cmp.get('c.fetch'); helper.load(); }, load: function() {} })",
        ),
        (
            renderer,
            "({ render: function(cmp, event, helper) { helper.load(); } })",
        ),
        (
            "aura/Orders/Orders.cmp",
            "<aura:component controller=\"OrderController\"/>",
        ),
        (
            "classes/OrderController.cls",
            "public class OrderController { @AuraEnabled public static void fetch() {} }",
        ),
    ]);
    targets(
        reference(&facts, (controller, "helper.invoke", ReferenceKind::Calls)),
        capability_symbol(&facts, helper, &format!("{helper}::invoke")),
        "framework-salesforce-client-action",
    );
    for path in [helper, renderer] {
        assert!(
            reference(&facts, (path, "helper.load", ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
    let calls = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "c.fetch")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    targets(
        calls[0],
        capability_symbol(
            &facts,
            "classes/OrderController.cls",
            "OrderController::fetch",
        ),
        "framework-salesforce-server-action",
    );
}

#[test]
fn entity_handler_values_cannot_acquire_php_class_lookup() {
    let path = "demo.routing.yml";
    let facts = generation(&[
        (
            path,
            "list:\n  path: /nodes\n  defaults:\n    _entity_list: node\nview:\n  path: /view\n  defaults:\n    _entity_view: node.full\nform:\n  path: /edit\n  defaults:\n    _entity_form: node.edit\n",
        ),
        ("src/Unrelated.php", "<?php class node {}"),
    ]);
    for name in ["node", "node.full", "node.edit"] {
        assert!(
            reference(&facts, (path, name, ReferenceKind::References))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn class_intent_never_becomes_a_same_named_service_after_php_abstains() {
    let path = "demo.services.yml";
    for classes in [
        Vec::new(),
        vec![
            ("src/Thing.php", "<?php namespace Vendor; class Thing {}"),
            ("other/Thing.php", "<?php namespace Vendor; class Thing {}"),
        ],
    ] {
        let mut fixtures = vec![(
            path,
            "services:\n  Vendor\\Thing:\n    class: Vendor\\Thing\n  alias:\n    alias: Vendor\\Thing\n",
        )];
        fixtures.extend(classes);
        let facts = generation(&fixtures);
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
        assert_eq!(uses.len(), 2);
        for reference in uses {
            if reference.owner_symbol_id.as_ref() == Some(&service.symbol_id) {
                assert!(reference.target_symbol_id.is_none(), "{reference:?}");
            } else {
                targets(reference, service, "framework-drupal-resource");
            }
        }
    }
}

#[test]
fn repeated_service_ids_across_installations_have_linear_lookup_work() {
    const SMALL_ROOTS: usize = 64;
    const LARGE_ROOTS: usize = SMALL_ROOTS * 2;
    const POLL_OVERHEAD: u64 = 256;
    let small = repeated_service_work(SMALL_ROOTS);
    let large = repeated_service_work(LARGE_ROOTS);
    assert!(
        large <= small * 2 + POLL_OVERHEAD,
        "service lookup work {small} -> {large}"
    );
}

fn repeated_service_work(count: usize) -> u64 {
    const CANCELLATION_TAIL_POLLS: u64 = 8;
    let sources = (0..count)
        .map(|index| {
            (
                format!("sites/site_{index}/modules/demo/demo.services.yml"),
                "services:\n  shared: ~\n  alias:\n    alias: shared\n",
            )
        })
        .collect::<Vec<_>>();
    let fixtures = sources
        .iter()
        .map(|(path, source)| (path.as_str(), *source))
        .collect::<Vec<_>>();
    let (result, polls) = counted_resolution(&fixtures, None);
    let (facts, _) = result.unwrap_or_else(|_| panic!("resolution failed"));
    let files = facts
        .symbols
        .iter()
        .map(|symbol| (&symbol.symbol_id, &symbol.file_id))
        .collect::<std::collections::HashMap<_, _>>();
    let aliases = facts
        .references
        .iter()
        .filter(|reference| reference.reference_name == "shared")
        .collect::<Vec<_>>();
    assert_eq!(aliases.len(), count);
    for reference in aliases {
        let target = reference
            .target_symbol_id
            .as_ref()
            .unwrap_or_else(|| panic!("missing scoped target"));
        assert_eq!(files.get(target).copied(), Some(&reference.file_id));
        assert_eq!(reference.resolution_provenance, "framework-drupal-resource");
    }
    let stop = polls.saturating_sub(CANCELLATION_TAIL_POLLS);
    let (cancelled, actual) = counted_resolution(&fixtures, Some(stop));
    assert!(cancelled.is_err());
    assert_eq!(actual, stop);
    polls
}

#[test]
fn top_level_controller_routes_keep_the_base_qualified_resolution() {
    const BASE_CONTROLLER_CONFIDENCE: f32 = 0.90;
    let path = "config/shop.routing.yml";
    let facts = generation(&[
        (
            path,
            "show:\n  path: /show\n  controller: '\\OrderController::show'\nmissing:\n  path: /missing\n  controller: '\\MissingController::show'\n",
        ),
        (
            "src/OrderController.php",
            "<?php class OrderController { public function show() {} }",
        ),
    ]);
    let resolved = reference(
        &facts,
        (path, "\\OrderController::show", ReferenceKind::Calls),
    );
    targets(
        resolved,
        capability_symbol(&facts, "src/OrderController.php", "OrderController::show"),
        "framework-php-controller-qualified",
    );
    assert_eq!(resolved.confidence, BASE_CONTROLLER_CONFIDENCE);
    assert!(
        reference(
            &facts,
            (path, "\\MissingController::show", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn dotted_include_files_keep_the_base_module_prefix() {
    let path = "modules/demo/demo.hooks.inc";
    let facts = generation(&[(
        path,
        "<?php function demo_help() {} function other_help() {}",
    )]);
    let contract = capability_symbol(&facts, path, &format!("{path}::drupal-hook:hook_help"));
    let resolved = reference(&facts, (path, "hook_help", ReferenceKind::References));
    targets(resolved, contract, "framework-drupal-resource");
    assert_eq!(
        resolved.owner_symbol_id.as_ref(),
        Some(&capability_symbol(&facts, path, "demo_help").symbol_id)
    );
    assert_eq!(
        facts
            .references()
            .iter()
            .filter(|reference| reference.reference_name == "hook_help")
            .count(),
        1
    );
}

#[test]
fn cross_file_service_aliases_keep_the_base_exact_confidence_and_provenance() {
    let consumer = "modules/consumer/consumer.services.yml";
    let provider = "modules/provider/provider.services.yml";
    let facts = generation(&[
        (consumer, "services:\n  consumer:\n    alias: demo.base\n"),
        (provider, "services:\n  demo.base: ~\n"),
    ]);
    let resolved = reference(&facts, (consumer, "demo.base", ReferenceKind::References));
    targets(
        resolved,
        capability_symbol(
            &facts,
            provider,
            &format!("{provider}::drupal-service::demo.base"),
        ),
        EXACT_PROJECT_PROVENANCE,
    );
    assert_eq!(resolved.confidence, EXACT_PROJECT_CONFIDENCE);
}
