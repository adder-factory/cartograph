//! Verifier counterexamples retain written scope and existing exact targets.

use super::*;

const PROPERTY_WRITE_CASES: &[(&str, &str)] = &[
    (
        "",
        "$this->user_model = new Other_model(); $this->user_model->find();",
    ),
    ("public $user_model;", "$this->user_model->find();"),
    (
        "public Other_model $user_model;",
        "$this->user_model->find();",
    ),
    (
        "",
        "$this->load->model('user_model', 'active'); $this->active = new Other_model(); $this->active->find();",
    ),
    (
        "public $active;",
        "$this->load->model('user_model', 'active'); $this->active->find();",
    ),
    (
        "",
        "$this->user_model->find(); $this->user_model = new Other_model();",
    ),
    (
        "",
        "[$this->user_model] = [new Other_model()]; $this->user_model->find();",
    ),
    (
        "",
        "list($this->user_model) = [new Other_model()]; $this->user_model->find();",
    ),
    (
        "",
        "foreach ([new Other_model()] as $this->user_model) {} $this->user_model->find();",
    ),
    (
        "",
        "foreach ([new Other_model()] as $key => &$this->user_model) {} $this->user_model->find();",
    ),
    (
        "",
        "foreach ([[new Other_model()]] as [$this->user_model]) {} $this->user_model->find();",
    ),
    (
        "",
        "[[[[[$this->user_model]]]]] = [[[[[new Other_model()]]]]]; $this->user_model->find();",
    ),
    (
        "",
        "($this)->user_model = new Other_model(); $this->user_model->find();",
    ),
    (
        "",
        "(/* comment */ $this)->user_model = new Other_model(); $this->user_model->find();",
    ),
    (
        "",
        "((((($this)))))->user_model = new Other_model(); $this->user_model->find();",
    ),
    (
        "",
        "$this->load->model('user_model', 'active'); [$this->active] = [new Other_model()]; $this->active->find();",
    ),
];

#[test]
fn mybatis_one_component_written_namespaces_never_expand_to_packages() {
    let caller =
        "<mapper namespace=\"p.Caller\"><select id=\"s\" resultMap=\"AuditMapper.R\"/></mapper>";
    for namespace in ["p.AuditMapper", "AuditMapper"] {
        let mapper = format!(
            "<mapper namespace=\"{namespace}\"><resultMap id=\"R\" type=\"java.lang.String\"/></mapper>"
        );
        let facts = generation(&[("caller.xml", caller), ("target.xml", &mapper)]);
        let dependency = reference(
            &facts,
            ("caller.xml", "AuditMapper::R", ReferenceKind::References),
        );
        if namespace == "AuditMapper" {
            targets(
                dependency,
                capability_symbol(&facts, "target.xml", "AuditMapper::R"),
                "framework-mybatis-template-qualified-namespace",
            );
        } else {
            assert!(dependency.target_symbol_id.is_none());
        }
    }
}

#[test]
fn mybatis_local_sql_fragments_keep_exact_targets_among_other_namespaces() {
    let local = "<mapper namespace=\"a.OrderMapper\"><sql id=\"cols\">id</sql><select id=\"find\"><include refid=\"cols\"/></select></mapper>";
    let other = "<mapper namespace=\"b.OrderMapper\"><sql id=\"cols\">id</sql></mapper>";
    for fixtures in [
        vec![("a.xml", local)],
        vec![("a.xml", local), ("b.xml", other)],
    ] {
        let facts = generation(&fixtures);
        let dependency = reference(
            &facts,
            ("a.xml", "OrderMapper::cols", ReferenceKind::References),
        );
        targets(
            dependency,
            capability_symbol(&facts, "a.xml", "OrderMapper::cols"),
            EXACT_SAME_FILE_PROVENANCE,
        );
        assert_eq!(dependency.confidence, EXACT_SAME_FILE_CONFIDENCE);
    }
    let missing = generation(&[
        (
            "a.xml",
            "<mapper namespace=\"a.OrderMapper\"><select id=\"find\"><include refid=\"cols\"/></select></mapper>",
        ),
        ("b.xml", other),
    ]);
    assert!(
        reference(
            &missing,
            ("a.xml", "OrderMapper::cols", ReferenceKind::References)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn codeigniter_property_declarations_and_writes_veto_resource_conventions() {
    for &(declaration, body) in PROPERTY_WRITE_CASES {
        let source = format!(
            "<?php class X extends CI_Model {{ {declaration} public function run() {{ {body} }} }}"
        );
        let facts = generation(&[
            ("application/models/X.php", &source),
            (
                "application/models/User_model.php",
                "<?php class User_model { public function find() {} }",
            ),
            (
                "application/models/Other_model.php",
                "<?php class Other_model { public function find() {} }",
            ),
        ]);
        let rejected = capability_symbol(
            &facts,
            "application/models/User_model.php",
            "User_model::find",
        );
        assert!(
            facts
                .references()
                .iter()
                .filter(|site| site.reference_kind == ReferenceKind::Calls.as_str())
                .all(|site| site.target_symbol_id.as_ref() != Some(&rejected.symbol_id)),
            "{source}"
        );
        assert!(facts.references().iter().all(|site| site.resolution_provenance != "framework-codeigniter-inferred-resource"), "{source}");
    }
    let inferred = generation(&[
        (
            "application/models/X.php",
            "<?php class X extends CI_Model { public function run() { $this->user_model->find(); } }",
        ),
        (
            "application/models/User_model.php",
            "<?php class User_model { public function find() {} }",
        ),
    ]);
    targets(
        reference(
            &inferred,
            ("application/models/X.php", "find", ReferenceKind::Calls),
        ),
        capability_symbol(
            &inferred,
            "application/models/User_model.php",
            "User_model::find",
        ),
        "framework-codeigniter-inferred-resource",
    );
    assert_eq!(
        reference(
            &inferred,
            ("application/models/X.php", "find", ReferenceKind::Calls)
        )
        .confidence,
        INFERRED_RESOURCE_CONFIDENCE
    );
}

#[test]
fn codeigniter_routes_require_the_written_directory_and_owning_application() {
    let source = "<?php $route['shop'] = 'catalog/show/42';";
    let controller = "<?php class Catalog { public function show() {} }";
    for path in [
        "application/controllers/admin/Catalog.php",
        "other/application/controllers/Catalog.php",
    ] {
        let facts = generation(&[
            ("application/config/routes.php", source),
            (path, controller),
        ]);
        assert!(
            reference(
                &facts,
                (
                    "application/config/routes.php",
                    "catalog/show/42",
                    ReferenceKind::Calls
                )
            )
            .target_symbol_id
            .is_none(),
            "{path}: {:?}",
            reference(
                &facts,
                (
                    "application/config/routes.php",
                    "catalog/show/42",
                    ReferenceKind::Calls
                )
            )
        );
    }
    let facts = generation(&[
        ("app/application/config/routes.php", source),
        ("app/application/controllers/Catalog.php", controller),
        ("other/application/controllers/Catalog.php", controller),
    ]);
    targets(
        reference(
            &facts,
            (
                "app/application/config/routes.php",
                "catalog/show/42",
                ReferenceKind::Calls,
            ),
        ),
        capability_symbol(
            &facts,
            "app/application/controllers/Catalog.php",
            "Catalog::show",
        ),
        "framework-codeigniter-route-target",
    );
}

#[test]
fn codeigniter_controller_loads_preserve_the_written_path_and_resource_kind() {
    let source = "<?php class X extends CI_Controller { public function run() { $this->load->model ('blog/audit_model', 'active'); $this->active->log(); } }";
    let model = "<?php class Audit_model { public function log() {} }";
    let missing = generation(&[
        ("application/controllers/X.php", source),
        ("application/models/Audit_model.php", model),
    ]);
    assert!(
        missing
            .references()
            .iter()
            .filter(|site| site.reference_kind == ReferenceKind::Calls.as_str()
                && site.reference_name.ends_with("log"))
            .all(|site| site.target_symbol_id.is_none())
    );
    let facts = generation(&[
        ("application/controllers/X.php", source),
        ("application/models/Audit_model.php", model),
        ("application/models/blog/Audit_model.php", model),
        ("application/libraries/blog/Audit_model.php", model),
    ]);
    targets(
        reference(
            &facts,
            (
                "application/controllers/X.php",
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
            ("application/controllers/X.php", "log", ReferenceKind::Calls),
        ),
        capability_symbol(
            &facts,
            "application/models/blog/Audit_model.php",
            "Audit_model::log",
        ),
        "framework-codeigniter-loaded-resource",
    );
}

#[test]
fn codeigniter_nested_application_loads_do_not_bind_outer_models() {
    let path = "application/vendor/blog/application/controllers/X.php";
    let source = "<?php class X extends CI_Controller { public function run() { $this->load->model('audit_model', 'active'); $this->active->log(); } }";
    let model = "<?php class Audit_model { public function log() {} }";
    let missing = generation(&[
        (path, source),
        ("application/models/Audit_model.php", model),
    ]);
    assert!(
        reference(&missing, (path, "log", ReferenceKind::Calls))
            .target_symbol_id
            .is_none()
    );
    let facts = generation(&[
        (path, source),
        ("application/models/Audit_model.php", model),
        (
            "application/vendor/blog/application/models/Audit_model.php",
            model,
        ),
    ]);
    targets(
        reference(&facts, (path, "log", ReferenceKind::Calls)),
        capability_symbol(
            &facts,
            "application/vendor/blog/application/models/Audit_model.php",
            "Audit_model::log",
        ),
        "framework-codeigniter-loaded-resource",
    );
}

#[test]
fn flutter_local_route_classes_keep_exact_resolution_despite_import_combinators() {
    let source = "import 'package:flutter/material.dart'; import 'dart:async' show Future; class Home extends StatelessWidget { Widget build(BuildContext c) => const SizedBox(); } Widget app() => MaterialApp(routes: {'/': (_) => Home()});";
    let facts = generation(&[("lib/router.dart", source)]);
    let call = route_call(&facts, "Home");
    targets(
        call,
        capability_symbol(&facts, "lib/router.dart", "Home"),
        EXACT_SAME_FILE_PROVENANCE,
    );
    assert_eq!(call.confidence, EXACT_SAME_FILE_CONFIDENCE);
    let hidden = generation(&[
        (
            "lib/router.dart",
            "import 'screens.dart' hide Home; Widget app() => MaterialApp(routes: {'/': (_) => Home()});",
        ),
        ("lib/screens.dart", "class Home {}"),
    ]);
    assert!(route_call(&hidden, "Home").target_symbol_id.is_none());
}

#[test]
fn flutter_generic_route_constructors_retain_the_exact_base_class() {
    for arguments in ["int", "Map<String,List<int>>"] {
        let source = format!(
            "import 'package:flutter/material.dart'; class Page<T> extends StatelessWidget {{ Widget build(BuildContext c) => const SizedBox(); }} Widget app() => MaterialApp(routes: {{'/': (_) => Page<{arguments}>()}});"
        );
        let facts = generation(&[("lib/router.dart", &source)]);
        let call = route_call(&facts, "Page");
        targets(
            call,
            capability_symbol(&facts, "lib/router.dart", "Page"),
            EXACT_SAME_FILE_PROVENANCE,
        );
        assert_eq!(call.confidence, EXACT_SAME_FILE_CONFIDENCE);
    }
    let missing = generation(&[
        (
            "lib/router.dart",
            "import 'package:flutter/material.dart'; Widget app() => MaterialApp(routes: {'/': (_) => Page<int>()});",
        ),
        ("lib/unimported.dart", "class Page<T> {}"),
    ]);
    assert!(route_call(&missing, "Page").target_symbol_id.is_none());
}

fn route_call<'facts>(
    facts: &'facts CanonicalGenerationFacts,
    name: &str,
) -> &'facts ReferenceInput {
    let route = facts
        .symbols()
        .iter()
        .find(|symbol| symbol.symbol_kind == SymbolKind::Route.as_str())
        .unwrap_or_else(|| panic!("missing route"));
    let calls = facts
        .references()
        .iter()
        .filter(|site| {
            site.owner_symbol_id.as_ref() == Some(&route.symbol_id)
                && site.reference_kind == ReferenceKind::Calls.as_str()
                && site.reference_name == name
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    calls[0]
}
