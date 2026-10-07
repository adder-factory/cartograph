//! Framework bindings must identify one target and preserve uncertainty.

mod drupalsf;
mod drupalsf_repairs;
mod repairs;

use super::*;

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reversed = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reversed.digest());
    assert_eq!(forward.references(), reversed.references());
    assert_eq!(forward.edges(), reversed.edges());
    forward
}

fn counted_resolution(
    fixtures: &[(&str, &str)],
    cancel_after: Option<u64>,
) -> (
    Result<(GenerationFacts, ResolutionReport), ResolveGenerationFailure>,
    u64,
) {
    let limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("invalid limits: {error}"));
    let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
    for (path, source) in fixtures {
        let snapshot = cartograph_extract::SourceSnapshot::from_bytes_for_capability_validation(
            path,
            source.as_bytes(),
            limits,
        )
        .unwrap_or_else(|error| panic!("invalid fixture: {error}"));
        let file = NativeExtractor::new_for_capability_validation(snapshot.language())
            .and_then(|mut extractor| extractor.extract(&snapshot))
            .unwrap_or_else(|error| panic!("extraction failed: {error}"));
        accumulator
            .push(file)
            .unwrap_or_else(|_| panic!("fixture exceeds budget"));
    }
    let mut polls = 0_u64;
    let result = resolve_generation(
        ResolveGenerationRequest {
            extracted: accumulator,
            maximum_bytes: TEST_GENERATION_BYTES,
            source_root: test_source_root(),
            evidence_policy: FULL_TEST_EVIDENCE,
            clone_policy: NativeClonePolicy {
                wider_partial_band: false,
            },
        },
        || {
            polls += 1;
            cancel_after.is_some_and(|stop| polls >= stop)
        },
    );
    (result, polls)
}

fn reference<'facts>(
    facts: &'facts CanonicalGenerationFacts,
    site: (&str, &str, ReferenceKind),
) -> &'facts ReferenceInput {
    let (path, name, kind) = site;
    let file = facts
        .files()
        .iter()
        .find(|file| file.normalized_path == path)
        .unwrap_or_else(|| panic!("missing file {path}"));
    let matches = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.file_id == file.file_id
                && reference.reference_name == name
                && reference.reference_kind == kind.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "{site:?}: {matches:?}");
    matches[0]
}

fn targets(reference: &ReferenceInput, target: &SymbolInput, provenance: &str) {
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{reference:?}"
    );
    assert_eq!(reference.resolution_provenance, provenance);
}

#[test]
fn cargo_without_consumer_bindings_does_not_guess_crate_roots() {
    let facts = generation(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        (
            "crates/both/Cargo.toml",
            "[package]\nname = \"both-crate\"\n",
        ),
        ("crates/both/src/lib.rs", "pub fn library() {}\n"),
        ("crates/both/src/main.rs", "pub fn main() {}\n"),
        (
            "crates/bin/Cargo.toml",
            "[package]\nname = \"binary-crate\"\n",
        ),
        ("crates/bin/src/main.rs", "pub fn run() {}\n"),
        (
            "crates/mod/Cargo.toml",
            "[package]\nname = \"module-crate\"\n",
        ),
        ("crates/mod/src/mod.rs", "pub fn load() {}\n"),
        (
            "src/lib.rs",
            "use both_crate;\nuse binary_crate;\nuse module_crate;\nuse unknown_crate;\nuse binary_crate::run;\nuse both_crate::library;\nfn caller() { run(); library(); }\n",
        ),
    ]);
    for name in ["both_crate", "binary_crate", "module_crate"] {
        assert!(
            reference(&facts, ("src/lib.rs", name, ReferenceKind::Imports))
                .target_symbol_id
                .is_none()
        );
    }
    assert!(
        reference(
            &facts,
            ("src/lib.rs", "unknown_crate", ReferenceKind::Imports)
        )
        .target_symbol_id
        .is_none()
    );
    assert!(
        reference(&facts, ("src/lib.rs", "run", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
    targets(
        reference(&facts, ("src/lib.rs", "library", ReferenceKind::Calls)),
        capability_symbol(&facts, "crates/both/src/lib.rs", "library"),
        RUST_WORKSPACE_CRATE_PROVENANCE,
    );
}

#[test]
fn cargo_duplicate_package_names_remain_unresolved() {
    let facts = generation(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n"),
        ("a/Cargo.toml", "[package]\nname = \"duplicate\"\n"),
        ("b/Cargo.toml", "[package]\nname = \"duplicate\"\n"),
        ("a/src/main.rs", "pub fn run() {}\n"),
        ("b/src/main.rs", "pub fn run() {}\n"),
        ("src/lib.rs", "use duplicate;\nuse duplicate::run;\n"),
    ]);
    assert!(
        reference(&facts, ("src/lib.rs", "duplicate", ReferenceKind::Imports))
            .target_symbol_id
            .is_none()
    );
    assert!(
        reference(
            &facts,
            ("src/lib.rs", "duplicate::run", ReferenceKind::Imports)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn cargo_crate_fallback_cannot_replace_local_modules_with_a_workspace_root() {
    for (local, file_path) in [
        ("mod toolkit;", "src/toolkit.rs"),
        ("mod toolkit { pub fn local() {} }", "src/unrelated.rs"),
    ] {
        let source = format!("{local}\nuse toolkit as tools;\n");
        let facts = generation(&[
            ("Cargo.toml", "[workspace]\nmembers = ['crates/toolkit']\n"),
            ("crates/toolkit/Cargo.toml", "[package]\nname = 'toolkit'\n"),
            ("crates/toolkit/src/lib.rs", "pub fn external() {}\n"),
            (file_path, "pub fn local() {}\n"),
            ("src/lib.rs", &source),
        ]);
        let alias = reference(&facts, ("src/lib.rs", "tools", ReferenceKind::References));
        assert!(
            alias.target_symbol_id.is_none(),
            "local declaration forbids the crate-root heuristic: {alias:?}"
        );
    }
}

#[test]
fn salesforce_lwc_imports_and_alias_calls_bind_only_to_the_named_apex_class() {
    let facts = generation(&[
        (
            "lwc/orders/orders.js",
            "import fetch from '@salesforce/apex/Orders.fetch';\nimport resume from '@salesforce/apexContinuation/Orders.resume';\nimport missing from '@salesforce/apex/Absent.fetch';\nexport function run() { fetch(); resume(); missing(); }\n",
        ),
        (
            "classes/Orders.cls",
            "public class Orders {\n @AuraEnabled public static void fetch() {}\n public static void resume() {}\n}\n",
        ),
        (
            "classes/Other.cls",
            "public class Other { @AuraEnabled public static void fetch() {} }\n",
        ),
    ]);
    for (name, method) in [
        ("@salesforce/apex/Orders.fetch", "fetch"),
        ("@salesforce/apexContinuation/Orders.resume", "resume"),
    ] {
        targets(
            reference(
                &facts,
                ("lwc/orders/orders.js", name, ReferenceKind::Imports),
            ),
            capability_symbol(&facts, "classes/Orders.cls", &format!("Orders::{method}")),
            "framework-salesforce-apex-method",
        );
    }
    for (name, method) in [("fetch", "fetch"), ("resume", "resume")] {
        targets(
            reference(&facts, ("lwc/orders/orders.js", name, ReferenceKind::Calls)),
            capability_symbol(&facts, "classes/Orders.cls", &format!("Orders::{method}")),
            "framework-salesforce-apex-method",
        );
    }
    assert!(
        reference(
            &facts,
            ("lwc/orders/orders.js", "missing", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn salesforce_prefers_one_aura_enabled_overload_and_abstains_on_class_or_method_ties() {
    let script =
        "import fetch from '@salesforce/apex/Orders.fetch';\nexport function run() { fetch(); }\n";
    let facts = generation(&[
        ("lwc/orders/orders.js", script),
        (
            "classes/Orders.cls",
            "public class Orders {\n @AuraEnabled public static void fetch() {}\n public static void fetch(Integer limit) {}\n}\n",
        ),
    ]);
    let target = capability_symbol_by(
        &facts,
        &facts
            .files()
            .iter()
            .find(|file| file.normalized_path == "classes/Orders.cls")
            .unwrap_or_else(|| panic!("missing Apex file"))
            .file_id,
        |symbol| symbol.qualified_name == "Orders::fetch" && symbol.start_line == 2,
    );
    targets(
        reference(
            &facts,
            ("lwc/orders/orders.js", "fetch", ReferenceKind::Calls),
        ),
        target,
        "framework-salesforce-apex-method",
    );
    for apex in [
        "public class Orders {\n @AuraEnabled public static void fetch() {}\n @AuraEnabled public static void fetch(Integer limit) {}\n}\n",
        "public class Orders {\n public static void fetch() {}\n public static void fetch(Integer limit) {}\n}\n",
        "public class Orders { @AuraEnabled public static void fetch() {} }\npublic class Orders { @AuraEnabled public static void fetch() {} }\n",
    ] {
        let ambiguous = generation(&[
            ("lwc/orders/orders.js", script),
            ("classes/Orders.cls", apex),
        ]);
        assert!(
            reference(
                &ambiguous,
                ("lwc/orders/orders.js", "fetch", ReferenceKind::Calls)
            )
            .target_symbol_id
            .is_none()
        );
        assert!(
            reference(
                &ambiguous,
                (
                    "lwc/orders/orders.js",
                    "@salesforce/apex/Orders.fetch",
                    ReferenceKind::Imports
                )
            )
            .target_symbol_id
            .is_none()
        );
    }
}

#[test]
fn salesforce_conflicting_extensions_and_ambiguous_components_remain_unresolved() {
    let facts = generation(&[
        (
            "pages/Edit.page",
            "<apex:page controller=\"A\" extensions=\"B\"><apex:commandButton action=\"{!save}\"/><c:card/></apex:page>\n",
        ),
        (
            "classes/A.cls",
            "public class A { @AuraEnabled public static void save() {} }\n",
        ),
        (
            "classes/B.cls",
            "public class B { @AuraEnabled public static void save() {} }\n",
        ),
        ("aura/card/card.cmp", "<aura:component/>\n"),
        ("other/aura/card/card.cmp", "<aura:component/>\n"),
    ]);
    for (name, kind) in [
        ("save", ReferenceKind::Calls),
        ("Card", ReferenceKind::References),
    ] {
        assert!(
            reference(&facts, ("pages/Edit.page", name, kind))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn repeated_salesforce_component_sites_resolve_with_linear_cancellable_work() {
    let mut poll_counts = Vec::new();
    for count in [128, 256] {
        let markup = format!(
            "<aura:component>\n{}</aura:component>\n",
            "<c:card/>\n".repeat(count)
        );
        let fixtures = [
            ("aura/Panel/Panel.cmp", markup.as_str()),
            ("aura/card/card.cmp", "<aura:component/>\n"),
        ];
        let (result, polls) = counted_resolution(&fixtures, None);
        let (facts, _) = result.unwrap_or_else(|_| panic!("resolution failed"));
        let card = facts
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "card")
            .unwrap_or_else(|| panic!("missing component"));
        let references = facts
            .references
            .iter()
            .filter(|reference| reference.reference_name == "Card")
            .collect::<Vec<_>>();
        assert_eq!(references.len(), count);
        for site in references {
            targets(site, card, "framework-salesforce-component-convention");
        }
        for stop in [32, polls.saturating_sub(8)] {
            let (cancelled, actual) = counted_resolution(&fixtures, Some(stop));
            assert_matches!(cancelled, Err(ResolveGenerationFailure { reason: None }));
            assert_eq!(actual, stop);
        }
        poll_counts.push(polls);
    }
    assert!(
        poll_counts[1] <= poll_counts[0] * 3,
        "doubling sites must not quadruple binding scans: {poll_counts:?}"
    );
}

#[test]
fn salesforce_controllers_scope_actions_and_components_keep_bundle_case() {
    let facts = generation(&[
        (
            "pages/Edit.page",
            "<apex:page controller=\"A\"><apex:commandButton action=\"{!save}\"/><c:accountCard/></apex:page>\n",
        ),
        (
            "pages/Missing.page",
            "<apex:page controller=\"ns.Absent\"><apex:commandButton action=\"{!save}\"/></apex:page>\n",
        ),
        (
            "pages/NoController.page",
            "<apex:page><apex:commandButton action=\"{!save}\"/></apex:page>\n",
        ),
        ("aura/accountCard/accountCard.cmp", "<aura:component/>\n"),
        (
            "classes/A.cls",
            "public class A { @AuraEnabled public static void save() {} }\n",
        ),
        (
            "classes/B.cls",
            "public class B { @AuraEnabled public static void save() {} }\n",
        ),
    ]);
    targets(
        reference(&facts, ("pages/Edit.page", "A", ReferenceKind::References)),
        capability_symbol(&facts, "classes/A.cls", "A"),
        "framework-salesforce-controller",
    );
    targets(
        reference(&facts, ("pages/Edit.page", "save", ReferenceKind::Calls)),
        capability_symbol(&facts, "classes/A.cls", "A::save"),
        "framework-salesforce-apex-method",
    );
    assert!(
        reference(
            &facts,
            ("pages/Edit.page", "AccountCard", ReferenceKind::References),
        )
        .target_symbol_id
        .is_none()
    );
    for path in ["pages/Missing.page", "pages/NoController.page"] {
        assert!(
            reference(&facts, (path, "save", ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn aura_client_actions_do_not_guess_server_methods_or_unrelated_functions() {
    let facts = generation(&[
        (
            "aura/Panel/Panel.cmp",
            "<aura:component controller=\"PanelController\"><aura:handler action=\"{!c.save}\"/></aura:component>\n",
        ),
        (
            "aura/Panel/PanelController.js",
            "function save() {}\n({ nested: { save() {} } });\n",
        ),
        (
            "classes/PanelController.cls",
            "public class PanelController { @AuraEnabled public static void save() {} }\n",
        ),
    ]);
    targets(
        reference(
            &facts,
            (
                "aura/Panel/Panel.cmp",
                "PanelController",
                ReferenceKind::References,
            ),
        ),
        capability_symbol(&facts, "classes/PanelController.cls", "PanelController"),
        "framework-salesforce-controller",
    );
    let action = reference(
        &facts,
        ("aura/Panel/Panel.cmp", "save", ReferenceKind::Calls),
    );
    assert!(action.target_symbol_id.is_none());
    assert_eq!(action.resolution_provenance, UNRESOLVED_PROVENANCE);
}

#[test]
fn play_handlers_with_arguments_resolve_by_package_class_and_method() {
    let facts = generation(&[
        (
            "conf/routes",
            "GET /users/:id controllers.Users.show(id: Long)\nPOST /admin controllers.Admin.list()\nGET /missing controllers.Users.missing()\n",
        ),
        (
            "app/controllers/Users.scala",
            "package controllers\nclass Users { def show(id: Long): Unit = {} }\n",
        ),
        (
            "app/controllers/Admin.java",
            "package controllers;\npublic class Admin { public void list() {} }\n",
        ),
        (
            "app/other/Users.scala",
            "package other\nclass Users { def show(id: Long): Unit = {}; def missing(): Unit = {} }\n",
        ),
    ]);
    for (handler, path, name) in [
        (
            "controllers.Users.show",
            "app/controllers/Users.scala",
            "controllers::Users::show",
        ),
        (
            "controllers.Admin.list",
            "app/controllers/Admin.java",
            "controllers::Admin::list",
        ),
    ] {
        targets(
            reference(&facts, ("conf/routes", handler, ReferenceKind::Calls)),
            capability_symbol(&facts, path, name),
            "framework-play-qualified-handler",
        );
    }
    assert!(
        reference(
            &facts,
            (
                "conf/routes",
                "controllers.Users.missing",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn play_overloads_duplicate_controllers_and_private_handlers_abstain() {
    for declaration in [
        "package controllers\nclass Users { def show(): Unit = {}; def show(id: Long): Unit = {} }\n",
        "package controllers\nclass Users { def show(): Unit = {} }; class Users { def show(): Unit = {} }\n",
        "package controllers\nclass Users { private def show(): Unit = {} }\n",
        "package controllers\nclass Users { protected def show(): Unit = {} }\n",
        "package controllers\nprivate class Users { def show(): Unit = {} }\n",
    ] {
        let facts = generation(&[
            ("conf/api.routes", "GET /users controllers.Users.show()\n"),
            ("app/controllers/Users.scala", declaration),
        ]);
        let handler = reference(
            &facts,
            (
                "conf/api.routes",
                "controllers.Users.show",
                ReferenceKind::Calls,
            ),
        );
        assert!(handler.target_symbol_id.is_none());
        assert_eq!(handler.resolution_provenance, UNRESOLVED_PROVENANCE);
    }
}

#[test]
fn php_route_missing_method_falls_back_only_to_the_named_class() {
    let facts = generation(&[
        (
            "config/routes.yaml",
            "show:\n  path: /show\n  controller: 'App\\Http\\OrderController::show'\nmissing:\n  path: /missing\n  controller: 'App\\Http\\OrderController::missing'\nwrong:\n  path: /wrong\n  controller: 'Absent\\OrderController::show'\nshort:\n  path: /short\n  controller: 'OrderController::show'\n",
        ),
        (
            "src/Http/OrderController.php",
            "<?php\nnamespace App\\Http;\nclass OrderController { public function show() {} }\n",
        ),
        (
            "src/Other/OrderController.php",
            "<?php\nnamespace App\\Other;\nclass OrderController { public function show() {} }\n",
        ),
    ]);
    targets(
        reference(
            &facts,
            (
                "config/routes.yaml",
                r"App\Http\OrderController::show",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(
            &facts,
            "src/Http/OrderController.php",
            r"App\Http::OrderController::show",
        ),
        "framework-php-controller-qualified",
    );
    let fallback = reference(
        &facts,
        (
            "config/routes.yaml",
            r"App\Http\OrderController::missing",
            ReferenceKind::Calls,
        ),
    );
    targets(
        fallback,
        capability_symbol(
            &facts,
            "src/Http/OrderController.php",
            r"App\Http::OrderController",
        ),
        "framework-php-controller-class-fallback",
    );
    assert!(fallback.confidence < FRAMEWORK_CONVENTION_CONFIDENCE);
    for name in [r"Absent\OrderController::show", "OrderController::show"] {
        assert!(
            reference(&facts, ("config/routes.yaml", name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn php_controller_routes_preserve_import_aliases_and_case_insensitive_names() {
    let facts = generation(&[
        (
            "routes/web.php",
            "<?php\nuse App\\Http\\OrderController as AliasController;\nuse Vendor\\Controller as ExternalController;\nRoute::get('/show', 'AliasController@show');\nRoute::get('/missing', 'AliasController@missing');\nRoute::get('/external', 'ExternalController@show');\n",
        ),
        (
            "src/Http/OrderController.php",
            "<?php\nnamespace App\\Http;\nclass OrderController { public function show() {} }\n",
        ),
        (
            "src/Other/OrderController.php",
            "<?php\nnamespace Other;\nclass AliasController { public function show() {} }\nclass ExternalController { public function show() {} }\n",
        ),
        (
            "config/routes.yaml",
            "mixed:\n  path: /mixed\n  controller: 'app\\http\\ordercontroller::SHOW'\n",
        ),
    ]);
    let method = capability_symbol(
        &facts,
        "src/Http/OrderController.php",
        r"App\Http::OrderController::show",
    );
    targets(
        reference(
            &facts,
            (
                "routes/web.php",
                "AliasController@show",
                ReferenceKind::Calls,
            ),
        ),
        method,
        IMPORT_BINDING_PROVENANCE,
    );
    targets(
        reference(
            &facts,
            (
                "routes/web.php",
                "AliasController@missing",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(
            &facts,
            "src/Http/OrderController.php",
            r"App\Http::OrderController",
        ),
        "framework-php-controller-class-fallback",
    );
    targets(
        reference(
            &facts,
            (
                "config/routes.yaml",
                r"app\http\ordercontroller::SHOW",
                ReferenceKind::Calls,
            ),
        ),
        method,
        "framework-php-controller-qualified",
    );
    assert!(
        reference(
            &facts,
            (
                "routes/web.php",
                "ExternalController@show",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn php_controller_fallback_cannot_bypass_private_members_or_unknown_ancestry() {
    let facts = generation(&[
        (
            "config/routes.yaml",
            "private:\n  path: /private\n  controller: 'PrivateController::show'\nunknown:\n  path: /unknown\n  controller: 'UnknownController::missing'\n",
        ),
        (
            "src/Controller.php",
            "<?php\nclass PrivateController { private function show() {} }\nclass UnknownController extends Vendor {}\n",
        ),
    ]);
    for name in ["PrivateController::show", "UnknownController::missing"] {
        assert!(
            reference(&facts, ("config/routes.yaml", name, ReferenceKind::Calls))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn php_lowercase_route_aliases_keep_external_targets_authoritative() {
    let facts = generation(&[
        (
            "routes/web.php",
            "<?php\nuse Vendor\\Controller as ExternalController;\nuse App\\Http\\OrderController as AliasController;\nRoute::get('/external', 'externalcontroller@show');\nRoute::get('/local', 'aliascontroller@index');\n",
        ),
        (
            "src/Controller.php",
            "<?php\nnamespace App\\Http;\nclass OrderController { public function index() {} }\n",
        ),
        (
            "src/Decoy.php",
            "<?php\nclass externalcontroller { public function show() {} }\n",
        ),
    ]);
    assert!(
        reference(
            &facts,
            (
                "routes/web.php",
                "externalcontroller@show",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
    targets(
        reference(
            &facts,
            (
                "routes/web.php",
                "aliascontroller@index",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(
            &facts,
            "src/Controller.php",
            r"App\Http::OrderController::index",
        ),
        "framework-php-controller-convention",
    );
}

#[test]
fn php_routes_cannot_apply_an_alias_from_another_namespace_block() {
    for source in [
        "<?php\nnamespace First { use App\\GoodController as AliasController; }\nnamespace Second { Route::get('/bad', 'AliasController@show'); }\n",
        "<?php\nnamespace { use App\\GoodController as AliasController; }\nnamespace { Route::get('/bad', 'AliasController@show'); }\n",
    ] {
        let facts = generation(&[
            ("routes/web.php", source),
            (
                "src/Good.php",
                "<?php\nnamespace App;\nclass GoodController { public function show() {} }\n",
            ),
        ]);
        assert!(
            reference(
                &facts,
                (
                    "routes/web.php",
                    "AliasController@show",
                    ReferenceKind::Calls
                )
            )
            .target_symbol_id
            .is_none()
        );
    }
}

#[test]
fn php_controller_routes_require_a_class_in_all_qualification_forms() {
    for declaration in [
        "interface BadController { public function show(); }",
        "trait BadController { public function show() {} }",
    ] {
        let php = format!("<?php\nnamespace App;\n{declaration}\n");
        let facts = generation(&[
            (
                "config/routes.yaml",
                "bad:\n  path: /bad\n  controller: 'App\\BadController::show'\n",
            ),
            (
                "routes/web.php",
                "<?php\nuse App\\BadController as AliasController;\nRoute::get('/bad', 'AliasController@show');\n",
            ),
            ("src/Bad.php", &php),
        ]);
        for (path, name) in [
            ("config/routes.yaml", r"App\BadController::show"),
            ("routes/web.php", "AliasController@show"),
        ] {
            assert!(
                reference(&facts, (path, name, ReferenceKind::Calls))
                    .target_symbol_id
                    .is_none()
            );
        }
    }
}

#[test]
fn rust_conventions_preserve_exact_project_and_go_package_resolution() {
    let facts = generation(&[
        ("src/handlers/ping.rs", "pub fn ping_handler() {}\n"),
        ("src/generated/ping.rs", "pub fn ping_handler() {}\n"),
        ("src/api/routes.rs", "pub fn duplicate_handler() {}\n"),
        (
            "src/handlers/duplicate.rs",
            "pub fn duplicate_handler() {}\n",
        ),
        (
            "other/src/handlers/external.rs",
            "pub fn external_handler() {}\n",
        ),
        (
            "src/lib.rs",
            "fn run() { ping_handler(); duplicate_handler(); external_handler(); }\n",
        ),
        (
            "handlers/local.go",
            "package handlers\nfunc LocalHandler() {}\n",
        ),
        (
            "handlers/use.go",
            "package handlers\nfunc run() { LocalHandler(); ExternalHandler() }\n",
        ),
        (
            "other/handlers/remote.go",
            "package handlers\nfunc ExternalHandler() {}\n",
        ),
    ]);
    targets(
        reference(&facts, ("src/lib.rs", "ping_handler", ReferenceKind::Calls)),
        capability_symbol(&facts, "src/handlers/ping.rs", "ping_handler"),
        FRAMEWORK_CONVENTION_PROVENANCE,
    );
    assert!(
        reference(
            &facts,
            ("src/lib.rs", "duplicate_handler", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
    let external = reference(
        &facts,
        ("src/lib.rs", "external_handler", ReferenceKind::Calls),
    );
    targets(
        external,
        capability_symbol(&facts, "other/src/handlers/external.rs", "external_handler"),
        EXACT_PROJECT_PROVENANCE,
    );
    assert_eq!(external.confidence, 0.95);
    targets(
        reference(
            &facts,
            ("handlers/use.go", "LocalHandler", ReferenceKind::Calls),
        ),
        capability_symbol(&facts, "handlers/local.go", "LocalHandler"),
        EXACT_PROJECT_PROVENANCE,
    );
    assert!(
        reference(
            &facts,
            ("handlers/use.go", "ExternalHandler", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn rust_services_and_models_use_directories_and_private_go_handlers_keep_package_scope() {
    let facts = generation(&[
        ("src/services/order.rs", "pub trait OrderService {}\n"),
        ("src/generated/order.rs", "pub trait OrderService {}\n"),
        ("src/models/account.rs", "pub struct Account {}\n"),
        ("src/generated/account.rs", "pub struct Account {}\n"),
        ("src/handlers/private.rs", "fn hidden_handler() {}\n"),
        (
            "src/lib.rs",
            "fn run(service: &dyn OrderService, account: Account) { hidden_handler(); }\n",
        ),
        (
            "handlers/local.go",
            "package handlers\nfunc hiddenHandler() {}\n",
        ),
        (
            "handlers/use.go",
            "package handlers\nfunc run() { hiddenHandler() }\n",
        ),
        (
            "foreign/handlers/local.go",
            "package handlers\nfunc hiddenHandler() {}\n",
        ),
    ]);
    for (name, path) in [
        ("OrderService", "src/services/order.rs"),
        ("Account", "src/models/account.rs"),
    ] {
        targets(
            reference(&facts, ("src/lib.rs", name, ReferenceKind::TypeOf)),
            capability_symbol(&facts, path, name),
            FRAMEWORK_CONVENTION_PROVENANCE,
        );
    }
    assert!(
        reference(
            &facts,
            ("src/lib.rs", "hidden_handler", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
    targets(
        reference(
            &facts,
            ("handlers/use.go", "hiddenHandler", ReferenceKind::Calls),
        ),
        capability_symbol(&facts, "handlers/local.go", "hiddenHandler"),
        FRAMEWORK_CONVENTION_PROVENANCE,
    );
}

#[test]
fn php_members_follow_exact_ancestry_and_traits_without_guessing_receivers() {
    let facts = generation(&[
        (
            "src/Base.php",
            "<?php\nnamespace App;\nclass Base { protected function authorize() {} public static function where() {} private function secret() {} }\ntrait Loggable { public function log() {} }\n",
        ),
        (
            "src/Child.php",
            "<?php\nnamespace App;\nclass Child extends Base { use Loggable; public function run() { $this->authorize(); $this->log(); $this->secret(); $unknown->log(); } }\nfunction run() { Child::where(); }\n",
        ),
    ]);
    let caller = capability_symbol(&facts, "src/Child.php", "App::Child::run");
    for (name, target) in [
        ("authorize", "App::Base::authorize"),
        ("log", "App::Loggable::log"),
    ] {
        let call = CapabilityReferenceQuery::new(&facts, caller)
            .named(&format!("$this->{name}"), ReferenceKind::Calls);
        targets(
            call,
            capability_symbol(&facts, "src/Base.php", target),
            DYNAMIC_DISPATCH_PROVENANCE,
        );
    }
    assert!(
        CapabilityReferenceQuery::new(&facts, caller)
            .named("$this->secret", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    assert!(
        CapabilityReferenceQuery::new(&facts, caller)
            .named("$unknown->log", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    targets(
        reference(
            &facts,
            ("src/Child.php", "Child::where", ReferenceKind::Calls),
        ),
        capability_symbol(&facts, "src/Base.php", "App::Base::where"),
        EXACT_PROJECT_PROVENANCE,
    );
}

#[test]
fn php_trait_conflicts_adaptations_unknown_parents_and_cycles_abstain() {
    let facts = generation(&[(
        "src/Traits.php",
        r"<?php
trait First { public function log() {} }
trait Second { public function log() {} }
class Conflict { use First, Second; public function run() { $this->log(); } }
class Adapted { use First { log as renamed; } public function run() { $this->log(); } }
class Unknown extends Vendor { public function run() { $this->log(); } }
class LoopA extends LoopB { public function run() { $this->log(); } }
class LoopB extends LoopA {}
class MissingTrait { use VendorTrait; public function run() { $this->log(); } }
",
    )]);
    for class in ["Conflict", "Adapted", "Unknown", "LoopA", "MissingTrait"] {
        let owner = capability_symbol(&facts, "src/Traits.php", &format!("{class}::run"));
        assert!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named("$this->log", ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn php_frozen_v1_corpus_members_reach_their_exact_declaring_files() {
    let facts = generation(&[
        (
            "app/Http/Controllers/UserController.php",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/php/app/Http/Controllers/UserController.php"
            ),
        ),
        (
            "app/Models/User.php",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/php/app/Models/User.php"
            ),
        ),
    ]);
    let index = capability_symbol(
        &facts,
        "app/Http/Controllers/UserController.php",
        r"App\Http\Controllers::UserController::index",
    );
    for (reference_name, path, name, provenance) in [
        (
            "$this->authorize",
            "app/Http/Controllers/UserController.php",
            r"App\Http\Controllers::BaseController::authorize",
            DYNAMIC_DISPATCH_PROVENANCE,
        ),
        (
            "User::where",
            "app/Models/User.php",
            r"App\Models::Model::where",
            EXACT_PROJECT_PROVENANCE,
        ),
        (
            "ApiClient::for()->createOrder",
            "app/Models/User.php",
            r"App\Models::ApiClient::createOrder",
            DYNAMIC_DISPATCH_PROVENANCE,
        ),
    ] {
        targets(
            CapabilityReferenceQuery::new(&facts, index)
                .named(reference_name, ReferenceKind::Calls),
            capability_symbol(&facts, path, name),
            provenance,
        );
    }
    let save = capability_symbol(&facts, "app/Models/User.php", r"App\Models::User::save");
    targets(
        CapabilityReferenceQuery::new(&facts, save).named("$this->log", ReferenceKind::Calls),
        capability_symbol(&facts, "app/Models/User.php", r"App\Models::Loggable::log"),
        DYNAMIC_DISPATCH_PROVENANCE,
    );
    assert!(
        CapabilityReferenceQuery::new(&facts, index)
            .named("$user->save", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}
