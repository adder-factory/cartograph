//! Wave 3 route targets through extraction, resolution and canonical reduction.

use std::fmt::Write as _;

mod repairs;

use super::*;

const CONVENTION_CONFIDENCE: f32 = 0.75;
const QUALIFIED_TEMPLATE_CONFIDENCE: f32 = 0.90;
const INFERRED_RESOURCE_CONFIDENCE: f32 = 0.70;
const DIRECTORY_ROUTE_CONFIDENCE: f32 = 0.80;
const UNSUPPORTED_NAMESPACE_BYTES: usize = 1_025;
const RESOURCE_LIMIT: usize = 256;
const PREVIOUS_CUSTOM_FAMILY_DIGEST: &str =
    "6ef2ddf7ec04c15deb9fbad64b9f080695c8048396e465c2115893ec03d6a64d";

#[test]
fn flutter_route_widgets_bind_to_the_explicit_import_and_keep_ambiguity() {
    let source = "import 'package:flutter/material.dart'; import 'screens.dart'; void setup() { final routes = [GoRoute(path: '/p', builder: (ctx, s) => Profile())]; final app = MaterialApp(routes: {'/s': (_) => Settings(title: 'a', subtitle: 'b')}); }";
    let facts = generation(&[
        ("lib/router.dart", source),
        ("lib/screens.dart", "class Profile {} class Settings {}"),
        ("other/screens.dart", "class Profile {} class Settings {}"),
    ]);
    for name in ["Profile", "Settings"] {
        targets(
            reference(&facts, ("lib/router.dart", name, ReferenceKind::References)),
            capability_symbol(&facts, "lib/screens.dart", name),
            "framework-flutter-relative-widget-fallback",
        );
        assert_eq!(
            reference(&facts, ("lib/router.dart", name, ReferenceKind::References)).confidence,
            CONVENTION_CONFIDENCE
        );
    }
    let ambiguous = generation(&[
        (
            "lib/router.dart",
            "import 'one.dart'; import 'two.dart'; void setup() { final r = GoRoute(path: '/p', builder: (c, s) => Profile()); }",
        ),
        ("lib/one.dart", "class Profile {}"),
        ("lib/two.dart", "class Profile {}"),
    ]);
    assert!(
        reference(
            &ambiguous,
            ("lib/router.dart", "Profile", ReferenceKind::References)
        )
        .target_symbol_id
        .is_none()
    );
    for imports in [
        "import 'one.dart' hide Profile;",
        "import 'one.dart' show Other;",
        "import 'one.dart' if (condition) 'two.dart';",
        "import 'one.dart'; import 'missing.dart';",
    ] {
        let router = format!(
            "{imports} void setup() {{ final r = GoRoute(path: '/p', builder: (c, s) => Profile()); }}"
        );
        let negative = generation(&[
            ("lib/router.dart", &router),
            ("lib/one.dart", "class Profile {}"),
            ("lib/two.dart", "class Profile {}"),
        ]);
        assert!(
            reference(
                &negative,
                ("lib/router.dart", "Profile", ReferenceKind::References)
            )
            .target_symbol_id
            .is_none(),
            "{imports}"
        );
    }
}

#[test]
fn spring_field_placeholders_resolve_unique_keys_without_selecting_a_profile() {
    let java =
        "package com.acme; public class Settings { @Value(\"${app.cache.ttl}\") private int ttl; }";
    let facts = generation(&[
        ("src/Settings.java", java),
        (
            "src/main/resources/application.properties",
            "app.cache.ttl=60\n",
        ),
    ]);
    targets(
        reference(
            &facts,
            (
                "src/Settings.java",
                "app.cache.ttl",
                ReferenceKind::References,
            ),
        ),
        capability_symbol(
            &facts,
            "src/main/resources/application.properties",
            "app.cache.ttl",
        ),
        "framework-spring-property",
    );
    let duplicate = generation(&[
        ("src/Settings.java", java),
        (
            "src/main/resources/application.properties",
            "app.cache.ttl=60\n",
        ),
        (
            "src/main/resources/application-dev.properties",
            "app.cache.ttl=5\n",
        ),
    ]);
    assert!(
        reference(
            &duplicate,
            (
                "src/Settings.java",
                "app.cache.ttl",
                ReferenceKind::References
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn mybatis_configuration_classes_keep_the_written_package() {
    let configuration = "<configuration><mappers><mapper class=\"com.acme.UserMapper\"/><mapper class=\"missing.UserMapper\"/></mappers></configuration>";
    let facts = generation(&[
        ("resources/mybatis-config.xml", configuration),
        (
            "java/com/acme/UserMapper.java",
            "package com.acme; public interface UserMapper {}",
        ),
        (
            "java/other/UserMapper.java",
            "package other; public interface UserMapper {}",
        ),
    ]);
    let references = facts
        .references()
        .iter()
        .filter(|reference| reference.reference_name == "UserMapper")
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 2);
    let target = capability_symbol(
        &facts,
        "java/com/acme/UserMapper.java",
        "com.acme::UserMapper",
    );
    let resolved = references
        .iter()
        .find(|reference| reference.target_symbol_id.is_some())
        .unwrap_or_else(|| panic!("missing mapper target"));
    targets(resolved, target, "framework-mybatis-configuration-class");
    assert_eq!(
        references
            .iter()
            .filter(|reference| reference.target_symbol_id.is_none())
            .count(),
        1
    );
}

#[test]
fn mybatis_template_aliases_resolve_without_requiring_public_visibility() {
    let caller = "<mapper namespace=\"com.acme.UserMapper\"><select id=\"find\" resultMap=\"AuditMapper.AuditResult\"/></mapper>";
    let alias =
        "<mapper namespace=\"AuditMapper\"><resultMap id=\"AuditResult\" type=\"User\"/></mapper>";
    let facts = generation(&[
        ("mapper/UserMapper.xml", caller),
        ("mapper/AuditMapper.xml", alias),
    ]);
    targets(
        reference(
            &facts,
            (
                "mapper/UserMapper.xml",
                "AuditMapper::AuditResult",
                ReferenceKind::References,
            ),
        ),
        capability_symbol(&facts, "mapper/AuditMapper.xml", "AuditMapper::AuditResult"),
        "framework-mybatis-template-qualified-namespace",
    );
    assert_eq!(
        reference(
            &facts,
            (
                "mapper/UserMapper.xml",
                "AuditMapper::AuditResult",
                ReferenceKind::References
            )
        )
        .confidence,
        QUALIFIED_TEMPLATE_CONFIDENCE
    );
    let duplicate = generation(&[
        ("mapper/UserMapper.xml", caller),
        ("mapper/AuditMapper.xml", alias),
        ("other/AuditMapper.xml", alias),
    ]);
    assert!(
        reference(
            &duplicate,
            (
                "mapper/UserMapper.xml",
                "AuditMapper::AuditResult",
                ReferenceKind::References
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn mybatis_missing_packages_cannot_retain_short_name_targets() {
    let configuration =
        "<configuration><mappers><mapper class=\"missing.UserMapper\"/></mappers></configuration>";
    for java in [
        "package com.acme; public interface UserMapper {}",
        "public interface UserMapper {}",
    ] {
        let facts = generation(&[
            ("mybatis-config.xml", configuration),
            ("UserMapper.java", java),
        ]);
        assert!(
            facts
                .references()
                .iter()
                .filter(|reference| reference.reference_name.contains("UserMapper"))
                .all(|reference| reference.target_symbol_id.is_none())
        );
    }
}

#[test]
fn mybatis_explicit_template_namespaces_require_the_written_mapper_owner() {
    let alias = "<mapper namespace=\"com.acme.AuditMapper\"><resultMap id=\"AuditResult\" type=\"User\"/></mapper>";
    for (namespace, expected) in [("com.acme", true), ("missing.pkg", false)] {
        let caller = format!(
            "<mapper namespace=\"com.acme.UserMapper\"><select id=\"find\" resultMap=\"{namespace}.AuditMapper.AuditResult\"/></mapper>"
        );
        let facts = generation(&[
            ("mapper/UserMapper.xml", &caller),
            ("mapper/AuditMapper.xml", alias),
        ]);
        let dependency = reference(
            &facts,
            (
                "mapper/UserMapper.xml",
                "AuditMapper::AuditResult",
                ReferenceKind::References,
            ),
        );
        if expected {
            targets(
                dependency,
                capability_symbol(&facts, "mapper/AuditMapper.xml", "AuditMapper::AuditResult"),
                "framework-mybatis-template-qualified-namespace",
            );
            assert_eq!(dependency.confidence, QUALIFIED_TEMPLATE_CONFIDENCE);
        } else {
            assert!(dependency.target_symbol_id.is_none());
        }
    }
}

#[test]
fn flutter_nested_closures_do_not_become_route_widget_targets() {
    let facts = generation(&[
        (
            "lib/router.dart",
            "import 'screens.dart'; void setup() { final route = GoRoute(path: '/x', builder: (c, s) { final thunk = () => Wrong(); return Correct(); }); }",
        ),
        ("lib/screens.dart", "class Wrong {} class Correct {}"),
    ]);
    let route = facts
        .symbols()
        .iter()
        .find(|symbol| symbol.symbol_kind == SymbolKind::Route.as_str())
        .unwrap_or_else(|| panic!("missing route"));
    assert!(
        facts
            .references()
            .iter()
            .all(|reference| reference.owner_symbol_id.as_ref() != Some(&route.symbol_id))
    );
}

#[test]
fn codeigniter_dynamic_loads_and_dynamic_route_methods_remain_unresolved() {
    let model = (
        "application/models/Audit_model.php",
        "<?php class Audit_model { public function log() {} }",
    );
    for statement in [
        "$this->load->model($dynamic ?? 'audit_model'); $this->audit_model->log();",
        r#"$this->load->model("$dynamic"); $this->audit_model->log();"#,
        r#"$this->load->model('audit_model', "$dynamic"); $this->audit_model->log();"#,
        "$this->load->model('é_model'); $this->audit_model->log();",
        "$this->load->model('audit_model', $dynamic ?? 'audit_model'); $this->audit_model->log();",
        "$text = \"$this->load->model('audit_model', 'active')\"; $this->active->log();",
    ] {
        let source =
            format!("<?php class X extends CI_Model {{ public function run() {{ {statement} }} }}");
        let facts = generation(&[("application/models/X.php", &source), model]);
        assert!(
            facts
                .references()
                .iter()
                .filter(|reference| reference.reference_name == "log")
                .all(|reference| reference.target_symbol_id.is_none())
        );
    }
    for target in ["catalog/$1", "catalog/42"] {
        let routes = format!("<?php $route['catalog/(:any)'] = '{target}';");
        let facts = generation(&[
            ("application/config/routes.php", &routes),
            (
                "application/controllers/Catalog.php",
                "<?php class Catalog { public function index() {} }",
            ),
        ]);
        assert!(
            reference(
                &facts,
                (
                    "application/config/routes.php",
                    target,
                    ReferenceKind::Calls
                )
            )
            .target_symbol_id
            .is_none()
        );
    }
}

#[test]
fn php_route_class_fallbacks_distinguish_external_ancestors_from_inaccessible_methods() {
    let route = "missing:\n  path: /missing\n  controller: 'App\\Http\\OrderController::missing'\n";
    let facts = generation(&[
        ("config/routes/admin.yaml", route),
        (
            "src/Http/OrderController.php",
            "<?php namespace App\\Http; class OrderController {}",
        ),
    ]);
    let dependency = reference(
        &facts,
        (
            "config/routes/admin.yaml",
            r"App\Http\OrderController::missing",
            ReferenceKind::Calls,
        ),
    );
    targets(
        dependency,
        capability_symbol(
            &facts,
            "src/Http/OrderController.php",
            r"App\Http::OrderController",
        ),
        "framework-php-controller-class-fallback",
    );
    assert_eq!(dependency.confidence, CONVENTION_CONFIDENCE);
    let private = generation(&[
        ("config/routes/admin.yaml", route),
        (
            "src/Http/OrderController.php",
            "<?php namespace App\\Http; class BaseController { private function missing() {} } class OrderController extends BaseController {}",
        ),
    ]);
    assert!(
        reference(
            &private,
            (
                "config/routes/admin.yaml",
                r"App\Http\OrderController::missing",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
    let unknown = generation(&[
        ("config/routes/admin.yaml", route),
        (
            "src/Http/OrderController.php",
            "<?php namespace App\\Http; class OrderController extends Vendor\\BaseController {}",
        ),
    ]);
    assert!(
        reference(
            &unknown,
            (
                "config/routes/admin.yaml",
                r"App\Http\OrderController::missing",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn php_adjacent_factory_assignment_uses_only_the_declared_return_type() {
    let model = "<?php namespace App; class User { public static function find(): ?self { return null; } public function save() {} }";
    let facts = generation(&[
        ("app/User.php", model),
        (
            "app/Caller.php",
            "<?php namespace App; function run() { $user = User::find(); $user->save(); }",
        ),
    ]);
    targets(
        reference(&facts, ("app/Caller.php", "save", ReferenceKind::Calls)),
        capability_symbol(&facts, "app/User.php", "App::User::save"),
        DYNAMIC_DISPATCH_PROVENANCE,
    );
    for source in [
        "<?php namespace App; function run() { $user = User::find(); $user = other(); $user->save(); }",
        "<?php namespace App; function run() { if ($flag) { $user = User::find(); } $user->save(); }",
        "<?php namespace App; function run() { $other = User::find(); $user->save(); }",
    ] {
        let negative = generation(&[("app/User.php", model), ("app/Caller.php", source)]);
        assert!(
            negative
                .references()
                .iter()
                .filter(|reference| reference.reference_name.contains("save"))
                .all(|reference| reference.target_symbol_id.is_none())
        );
    }
}

#[test]
fn codeigniter_explicit_loads_and_inferred_calls_resolve_only_matching_resource_files() {
    let caller = "<?php class X extends CI_Model { public function run() { $this->load->model('audit_model'); $this->audit_model->log(); $this->user_model->find(); } }";
    let facts = generation(&[
        ("application/models/X.php", caller),
        (
            "application/models/Audit_model.php",
            "<?php class Audit_model { public function log() {} }",
        ),
        (
            "application/models/User_model.php",
            "<?php class User_model { public function find() {} }",
        ),
        (
            "application/libraries/User_model.php",
            "<?php class User_model { public function find() {} }",
        ),
    ]);
    targets(
        reference(
            &facts,
            ("application/models/X.php", "log", ReferenceKind::Calls),
        ),
        capability_symbol(
            &facts,
            "application/models/Audit_model.php",
            "Audit_model::log",
        ),
        "framework-codeigniter-loaded-resource",
    );
    let inferred = reference(
        &facts,
        ("application/models/X.php", "find", ReferenceKind::Calls),
    );
    targets(
        inferred,
        capability_symbol(
            &facts,
            "application/models/User_model.php",
            "User_model::find",
        ),
        "framework-codeigniter-inferred-resource",
    );
    assert_eq!(inferred.confidence, INFERRED_RESOURCE_CONFIDENCE);
    let missing = generation(&[
        ("application/models/X.php", caller),
        (
            "application/libraries/User_model.php",
            "<?php class User_model { public function find() {} }",
        ),
    ]);
    assert!(
        reference(
            &missing,
            ("application/models/X.php", "find", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
    assert!(
        reference(
            &missing,
            ("application/models/X.php", "log", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
    let private = generation(&[
        ("application/models/X.php", caller),
        (
            "application/models/Audit_model.php",
            "<?php class Audit_model { private function log() {} }",
        ),
        (
            "application/libraries/Audit_model.php",
            "<?php class Audit_model { public function log() {} }",
        ),
    ]);
    assert!(
        reference(
            &private,
            ("application/models/X.php", "log", ReferenceKind::Calls)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn codeigniter_loaded_classes_and_subdirectory_calls_require_the_written_resource_path() {
    let caller = "<?php class X extends CI_Model { public function run() { $this->load->model('blog/audit_model', 'active'); $this->active->log(); } }";
    let model = "<?php class Audit_model { public function log() {} }";
    let facts = generation(&[
        ("application/models/X.php", caller),
        ("application/models/blog/Audit_model.php", model),
        ("application/models/Audit_model.php", model),
        ("application/libraries/blog/Audit_model.php", model),
    ]);
    targets(
        reference(
            &facts,
            (
                "application/models/X.php",
                "blog/audit_model",
                ReferenceKind::References,
            ),
        ),
        capability_symbol(
            &facts,
            "application/models/blog/Audit_model.php",
            "Audit_model",
        ),
        "framework-codeigniter-loaded-resource-class",
    );
    targets(
        reference(
            &facts,
            ("application/models/X.php", "log", ReferenceKind::Calls),
        ),
        capability_symbol(
            &facts,
            "application/models/blog/Audit_model.php",
            "Audit_model::log",
        ),
        "framework-codeigniter-loaded-resource",
    );
    let missing = generation(&[
        ("application/models/X.php", caller),
        ("application/models/Audit_model.php", model),
        ("application/libraries/blog/Audit_model.php", model),
    ]);
    for kind in [ReferenceKind::References, ReferenceKind::Calls] {
        let name = if kind == ReferenceKind::Calls {
            "log"
        } else {
            "blog/audit_model"
        };
        assert!(
            reference(&missing, ("application/models/X.php", name, kind))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn codeigniter_directory_targets_and_string_arguments_require_one_matching_split() {
    let route = "<?php $route['admin'] = 'admin/users/show';";
    let facts = generation(&[
        ("application/config/routes.php", route),
        (
            "application/controllers/admin/Users.php",
            "<?php class Users { public function show() {} }",
        ),
        (
            "application/controllers/Users.php",
            "<?php class Users { public function show() {} }",
        ),
    ]);
    let dependency = reference(
        &facts,
        (
            "application/config/routes.php",
            "admin/users/show",
            ReferenceKind::Calls,
        ),
    );
    targets(
        dependency,
        capability_symbol(
            &facts,
            "application/controllers/admin/Users.php",
            "Users::show",
        ),
        "framework-codeigniter-route-path",
    );
    assert_eq!(dependency.confidence, DIRECTORY_ROUTE_CONFIDENCE);
    let unrelated = generation(&[
        ("application/config/routes.php", route),
        (
            "application/controllers/Users.php",
            "<?php class Users { public function show() {} }",
        ),
    ]);
    assert!(
        reference(
            &unrelated,
            (
                "application/config/routes.php",
                "admin/users/show",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
    let tail = "<?php $route['shop'] = 'catalog/show/widget';";
    let controller = (
        "application/controllers/Catalog.php",
        "<?php class Catalog { public function show() {} }",
    );
    let exact = generation(&[("application/config/routes.php", tail), controller]);
    targets(
        reference(
            &exact,
            (
                "application/config/routes.php",
                "catalog/show/widget",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(&exact, controller.0, "Catalog::show"),
        "framework-codeigniter-route-path",
    );
    let ambiguous = generation(&[
        ("application/config/routes.php", tail),
        controller,
        (
            "application/controllers/catalog/Show.php",
            "<?php class Show { public function widget() {} }",
        ),
    ]);
    assert!(
        reference(
            &ambiguous,
            (
                "application/config/routes.php",
                "catalog/show/widget",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn codeigniter_argument_tails_and_default_index_fallback_keep_the_named_controller() {
    let routes =
        "<?php $route['shop'] = 'catalog/show/42'; $route['default_controller'] = 'welcome';";
    let facts = generation(&[
        ("application/config/routes.php", routes),
        (
            "application/controllers/Catalog.php",
            "<?php class Catalog { public function show() {} }",
        ),
        (
            "application/controllers/Welcome.php",
            "<?php class Welcome {}",
        ),
    ]);
    targets(
        reference(
            &facts,
            (
                "application/config/routes.php",
                "catalog/show/42",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(
            &facts,
            "application/controllers/Catalog.php",
            "Catalog::show",
        ),
        "framework-codeigniter-route-target",
    );
    let separate = generation(&[
        (
            "application/config/routes.php",
            "<?php $route['default_controller'] = 'welcome';",
        ),
        (
            "application/controllers/Welcome.php",
            "<?php class Welcome {}",
        ),
    ]);
    targets(
        reference(
            &separate,
            (
                "application/config/routes.php",
                "welcome",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(&separate, "application/controllers/Welcome.php", "Welcome"),
        "framework-codeigniter-controller-class-fallback",
    );
    let private = generation(&[
        (
            "application/config/routes.php",
            "<?php $route['default_controller'] = 'welcome';",
        ),
        (
            "application/controllers/Welcome.php",
            "<?php class Welcome { private function index() {} }",
        ),
    ]);
    assert!(
        reference(
            &private,
            (
                "application/config/routes.php",
                "welcome",
                ReferenceKind::Calls
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn mybatis_oversized_namespaces_cannot_bind_short_name_candidates() {
    let namespace = "x".repeat(UNSUPPORTED_NAMESPACE_BYTES);
    let configuration = format!(
        "<configuration><mappers><mapper class=\"{namespace}.UserMapper\"/></mappers></configuration>"
    );
    let caller = format!(
        "<mapper namespace=\"UserMapper\"><select id=\"find\" resultMap=\"{namespace}.AuditMapper.AuditResult\"/></mapper>"
    );
    let facts = generation(&[
        ("mybatis-config.xml", &configuration),
        ("UserMapper.java", "public interface UserMapper {}"),
        (
            "mapper/ClassCollision.xml",
            "<mapper namespace=\"cartograph.mybatis-unproven-class\"/>",
        ),
        (
            "mapper/TemplateCollision.xml",
            "<mapper namespace=\"cartograph.mybatis-unproven-template\"/>",
        ),
        ("mapper/UserMapper.xml", &caller),
        (
            "mapper/AuditMapper.xml",
            "<mapper namespace=\"AuditMapper\"><resultMap id=\"AuditResult\"/></mapper>",
        ),
    ]);
    for name in ["UserMapper", "AuditMapper::AuditResult"] {
        let references = facts
            .references()
            .iter()
            .filter(|reference| reference.reference_name == name)
            .collect::<Vec<_>>();
        assert_ne!(references.len(), 0);
        assert!(
            references
                .iter()
                .all(|reference| reference.target_symbol_id.is_none()),
            "{name}"
        );
    }
}

#[test]
fn mybatis_template_roles_reject_wrong_kinds_and_retain_sql_fragments() {
    let target = "<mapper namespace=\"com.acme.AuditMapper\"><resultMap id=\"AuditResult\"/><parameterMap id=\"Params\"/><sql id=\"fragment\"/><select id=\"statement\"/></mapper>";
    for namespace in ["com.acme.AuditMapper", "AuditMapper"] {
        let target = target.replace("com.acme.AuditMapper", namespace);
        let caller = format!(
            "<mapper namespace=\"com.acme.UserMapper\"><select id=\"wrongResult\" resultMap=\"{namespace}.fragment\"/><select id=\"wrongParameter\" parameterMap=\"{namespace}.AuditResult\"/><select id=\"wrongInclude\"><include refid=\"{namespace}.AuditResult\"/></select><select id=\"wrongStatement\"><include refid=\"{namespace}.statement\"/></select><select id=\"correct\"><include refid=\"{namespace}.fragment\"/></select></mapper>"
        );
        let facts = generation(&[
            ("mapper/UserMapper.xml", &caller),
            ("mapper/AuditMapper.xml", &target),
        ]);
        for owner in [
            "wrongResult",
            "wrongParameter",
            "wrongInclude",
            "wrongStatement",
            "correct",
        ] {
            let symbol = capability_symbol(
                &facts,
                "mapper/UserMapper.xml",
                &format!("UserMapper::{owner}"),
            );
            let dependency = facts
                .references()
                .iter()
                .find(|reference| {
                    reference.owner_symbol_id.as_ref() == Some(&symbol.symbol_id)
                        && reference.reference_kind == ReferenceKind::References.as_str()
                })
                .unwrap_or_else(|| panic!("missing reference from {owner}"));
            if owner == "correct" {
                let provenance = "framework-mybatis-template-qualified-namespace";
                targets(
                    dependency,
                    capability_symbol(&facts, "mapper/AuditMapper.xml", "AuditMapper::fragment"),
                    provenance,
                );
            } else {
                assert!(
                    dependency.target_symbol_id.is_none(),
                    "{namespace}: {owner}"
                );
            }
        }
    }
}

#[test]
fn mybatis_typed_lookups_cannot_bind_declarations_spelled_like_internal_hints() {
    let caller = "<mapper namespace=\"mybatis-template::AuditMapper\"><resultMap id=\"AuditResult::sql\"/><select id=\"find\"><include refid=\"AuditMapper.AuditResult\"/></select></mapper>";
    let configuration =
        "<configuration><mappers><mapper class=\"missing.UserMapper\"/></mappers></configuration>";
    let facts = generation(&[
        ("mapper/Caller.xml", caller),
        ("mybatis-config.xml", configuration),
        (
            "mapper/ClassCollision.xml",
            "<mapper namespace=\"mybatis-class::missing::UserMapper\"/>",
        ),
    ]);
    for (file, name) in [
        ("mapper/Caller.xml", "AuditMapper::AuditResult"),
        ("mybatis-config.xml", "UserMapper"),
    ] {
        assert!(
            reference(&facts, (file, name, ReferenceKind::References))
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn codeigniter_incomplete_load_tables_cannot_bind_retained_aliases() {
    let mut padding = String::new();
    for index in 1..RESOURCE_LIMIT {
        write!(padding, "$this->load->model('resource_{index}');")
            .unwrap_or_else(|error| panic!("resource load formatting: {error}"));
    }
    for tail in [
        "$this->load->model($dynamic, 'active');".to_owned(),
        format!("{padding}$this->load->model('second_model', 'active');"),
    ] {
        let source = format!(
            "<?php class X extends CI_Model {{ public function run() {{ $this->load->model('audit_model', 'active'); {tail} $this->active->log(); }} }}"
        );
        let facts = generation(&[
            ("application/models/X.php", &source),
            (
                "application/models/Audit_model.php",
                "<?php class Audit_model { public function log() {} }",
            ),
            (
                "application/models/Second_model.php",
                "<?php class Second_model { public function log() {} }",
            ),
        ]);
        assert!(
            facts
                .references()
                .iter()
                .filter(|reference| reference.reference_name == "log")
                .all(|reference| reference.target_symbol_id.is_none())
        );
    }
}

#[test]
fn codeigniter_backtick_load_text_cannot_resolve_real_calls() {
    for literal in [
        "`$this->load->model('audit_model', 'active');`",
        "`\n$this->load->model('audit_model', 'active');\n`",
    ] {
        let source = format!(
            "<?php class X extends CI_Model {{ public function run() {{ $text = {literal};\n$this->active->log(); }} }}"
        );
        let facts = generation(&[
            ("application/models/X.php", &source),
            (
                "application/models/Audit_model.php",
                "<?php class Audit_model { public function log() {} }",
            ),
        ]);
        assert!(
            facts
                .references()
                .iter()
                .filter(|reference| reference.reference_name == "log")
                .all(|reference| reference.target_symbol_id.is_none()),
            "{literal}"
        );
    }
}

#[test]
fn codeigniter_aliases_cannot_cross_class_or_free_function_contexts() {
    for extra in [
        "class Y extends CI_Model { public function two() { $this->active->log(); } }",
        "function two() { $this->active->log(); }",
    ] {
        let source = format!(
            "<?php class X extends CI_Model {{ public function one() {{ $this->load->model('audit_model', 'active'); }} }} {extra}"
        );
        let facts = generation(&[
            ("application/models/X.php", &source),
            (
                "application/models/Audit_model.php",
                "<?php class Audit_model { public function log() {} }",
            ),
        ]);
        assert!(
            facts
                .references()
                .iter()
                .filter(|reference| reference.reference_name == "log")
                .all(|reference| reference.target_symbol_id.is_none()),
            "{extra}"
        );
    }
}

#[test]
fn codeigniter_aliases_cannot_cross_anonymous_receiver_boundaries() {
    for context in [
        "new class { function two() { $this->active->log(); } }",
        "function () { $this->active->log(); }",
        "fn () => $this->active->log()",
    ] {
        let source = format!(
            "<?php class X extends CI_Model {{ function one() {{ $this->load->model('audit_model', 'active'); $other = {context}; }} }}"
        );
        let facts = generation(&[
            ("application/models/X.php", &source),
            (
                "application/models/Audit_model.php",
                "<?php class Audit_model { public function log() {} }",
            ),
        ]);
        targets(
            reference(
                &facts,
                (
                    "application/models/X.php",
                    "audit_model",
                    ReferenceKind::References,
                ),
            ),
            capability_symbol(&facts, "application/models/Audit_model.php", "Audit_model"),
            "framework-codeigniter-loaded-resource-class",
        );
        assert!(
            facts
                .references()
                .iter()
                .filter(|reference| reference.reference_name == "log")
                .all(|reference| reference.target_symbol_id.is_none()),
            "{context}"
        );
    }
}

#[test]
fn mybatis_local_sql_roles_preserve_the_frozen_custom_digest() {
    let fixtures = CUSTOM_FAMILY_FIXTURES
        .iter()
        .map(|(path, source, _)| (*path, *source))
        .collect::<Vec<_>>();
    let current = build_capability_generation(&fixtures, false);
    let mut removed_lookups = 0;
    let previous = generic_repair::build_generation(
        generic_repair::CapabilityGenerationRequest {
            fixtures: &fixtures,
            reverse: false,
            wider_partial_band: false,
            maximum_bytes: TEST_GENERATION_BYTES,
        },
        |file| {
            for reference in &mut file.references {
                if reference.resolution_name.as_deref()
                    == Some("mybatis-template-local::OrderMapper::orderColumns::sql")
                {
                    reference.resolution_name = None;
                    removed_lookups += 1;
                }
            }
        },
        || false,
    );
    assert_eq!(removed_lookups, 1);
    assert_unchanged_custom_facts(&current, &previous);
    assert_eq!(current.digest().as_str(), PREVIOUS_CUSTOM_FAMILY_DIGEST);
    assert_eq!(previous.digest().as_str(), PREVIOUS_CUSTOM_FAMILY_DIGEST);
}

fn assert_unchanged_custom_facts(
    current: &CanonicalGenerationFacts,
    previous: &CanonicalGenerationFacts,
) {
    assert_eq!(current.files(), previous.files());
    assert_eq!(current.symbols(), previous.symbols());
    assert_eq!(current.references(), previous.references());
    assert_eq!(current.edges(), previous.edges());
    assert_eq!(current.numerical_sites(), previous.numerical_sites());
    assert_eq!(current.documents(), previous.documents());
}
