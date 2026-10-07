//! Route details exercised through the production extractor.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_BYTES: usize = 1_048_576;

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_BYTES).unwrap_or_else(|error| panic!("limits: {error}"));
    let snapshot =
        SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot: {error}"));
    NativeExtractor::new_for_capability_validation(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot))
        .unwrap_or_else(|error| panic!("extract: {error}"))
}

fn routes(file: &ExtractedFile) -> Vec<&str> {
    let mut names = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

#[test]
fn aspnet_methods_without_a_class_route_and_stacked_attributes_keep_every_endpoint() {
    let file = extract(
        "Orders.cs",
        "class OrdersController { [HttpGet(\"/a\")][HttpPost(\"/b\")] public void Both() {} [HttpPost] public void Create() {} [Route(\"standalone\")] public void Landmark() {} [HttpGet(PATH)] public void Dynamic() {} [HttpGetExtra(\"/fake\")] public void Suffix() {} }",
    );
    assert_eq!(
        routes(&file),
        ["GET /a", "POST /", "POST /b", "ROUTE standalone"]
    );
    for (name, method) in [
        ("GET /a", "Both"),
        ("POST /b", "Both"),
        ("POST /", "Create"),
    ] {
        let route = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert!(
            file.references
                .iter()
                .any(|reference| reference.owner.as_ref() == Some(&route.id)
                    && reference.name == method
                    && reference.kind == ReferenceKind::Calls)
        );
    }
    assert!(
        file.references
            .iter()
            .all(|reference| reference.name != "Dynamic" || reference.kind != ReferenceKind::Calls)
    );
}

#[test]
fn spring_base_landmarks_belong_only_to_literal_class_mappings() {
    let java = extract(
        "Orders.java",
        "@RequestMapping(\"/api\") class Orders { @GetMapping(\"/orders\") void list() {} @RequestMapping(\"/method\") void method() {} }",
    );
    assert_eq!(
        routes(&java),
        ["ANY /api/method", "BASE /api", "GET /api/orders"]
    );
    let kotlin = extract(
        "Orders.kt",
        "@RequestMapping(\"/api\")\nclass Orders {\n @GetMapping(\"/orders\")\n fun list() {}\n}",
    );
    assert_eq!(routes(&kotlin), ["BASE /api", "GET /api/orders"]);
    assert!(
        routes(&extract(
            "Values.java",
            "@RequestMapping(\"/path/values\") class Values {}"
        ))
        .contains(&"BASE /path/values")
    );
    assert!(
        routes(&extract(
            "Other.java",
            "@RequestMapping(PATH) class Other { @GetMapping(\"/ok\") void ok() {} }"
        ))
        .iter()
        .all(|name| !name.starts_with("BASE"))
    );
}

#[test]
fn flask_blueprints_and_imported_python_routers_need_only_a_decorator_anchor() {
    let file = extract(
        "views.py",
        "from common import app\n@bp.route ('/users')\ndef users(): pass\n@app.get('/health')\ndef health(): pass\n@bp.route ('/write', methods=['POST', 'PUT'])\ndef write(): pass\n@bp.route('/unknown', methods=dynamic)\ndef unknown(): pass\n@bp.route(dynamic)\ndef dynamic_route(): pass\ntext = '@fake.route(\"/noise\")'\n",
    );
    assert_eq!(
        routes(&file),
        ["ANY /users", "GET /health", "POST /write", "PUT /write"]
    );
}

#[test]
fn go_handle_and_method_patterns_and_whitespace_need_no_import_hint() {
    let file = extract(
        "routes.go",
        "package routes\nfunc register(r Router) { r. GET (\"/x\", h); http.Handle(\"/static/\", fs); mux.HandleFunc(\"GET /api/users/{id}\", h); other.Handle(\"/wrong\", h); r.Get(\"key\", h); mux.Handle(unknown, h) }",
    );
    assert_eq!(
        routes(&file),
        ["ANY /static/", "GET /api/users/{id}", "GET /x"]
    );
}

#[test]
fn rocket_head_options_and_stacked_attributes_need_no_same_file_import() {
    let file = extract(
        "routes.rs",
        "#[head(\"/h\")] #[options(\"/o\")] fn both() {}\n#[get (\"/x\")] fn x() {}\n#[getter(\"/noise\")] fn noise() {}\nconst TEXT: &str = \"#[post(\\\"/fake\\\")]\";",
    );
    assert_eq!(routes(&file), ["GET /x", "HEAD /h", "OPTIONS /o"]);
}

#[test]
fn csharp_and_vapor_accept_whitespace_without_identifier_suffix_matches() {
    let file = extract(
        "Program.cs",
        "class Program { void Configure() { app.MapPut (\"/x\", Handle); app.OtherMapGet(\"/noise\", Handle); app.MapGet(path, \"/not_a_path\"); app.MapGet(\"/dynamic/\" + id, Handle); } }",
    );
    assert_eq!(routes(&file), ["PUT /x"]);
    let swift = extract(
        "routes.swift",
        "import Vapor\nfunc routes(_ app: Application) { app. get (\"/health\", use: health) }",
    );
    assert_eq!(routes(&swift), ["GET /health"]);
}

#[test]
fn rails_inline_namespace_end_and_multiple_verbs_keep_scope_and_literal_semicolons() {
    let file = extract(
        "config/routes.rb",
        "Rails.application.routes.draw do\nnamespace :api do; get '/x'; end\nget '/a'; post '/b'; get '/semi;colon'\nend\n",
    );
    assert_eq!(
        routes(&file),
        ["GET /a", "GET /api/x", "GET /semi;colon", "POST /b"]
    );
    assert_eq!(
        routes(&extract("lib/notes.rb", "get '/a'; post '/b'")),
        Vec::<&str>::new()
    );
}

#[test]
fn flutter_lists_maps_and_builder_parameters_preserve_paths_and_widget_identity() {
    let file = extract(
        "lib/router.dart",
        "void setup() { final router = GoRouter(routes: [GoRoute (path: '/', builder: (ctx, s) => Home(title: 'a', subtitle: 'b'), routes: [GoRoute(path: 'details', builder: (c, s) => Details())]), GoRoute(path: 'profile', builder: (ctx, s) => Profile())]); final app = MaterialApp(routes: {'/': (_) => Home(title: 'a', subtitle: 'b'), '/s': (_) => Settings()}); }",
    );
    assert_eq!(
        routes(&file),
        ["ANY /", "ANY /", "ANY /details", "ANY /profile", "ANY /s"]
    );
    let references = file
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::References
                && reference.owner.as_ref().is_some_and(|owner| {
                    file.symbols
                        .iter()
                        .any(|symbol| &symbol.id == owner && symbol.kind == SymbolKind::Route)
                })
        })
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        references,
        ["Home", "Details", "Profile", "Home", "Settings"]
    );
    let negative = extract(
        "lib/notes.dart",
        "void setup() { final data = {'routes': {'/x': 'Widget()'}}; final router = GoRoute(path: dynamicPath, builder: (ctx, s) => Wrong()); final fake = FakeMaterialApp(routes: {'/fake': (_) => Wrong()}); }",
    );
    assert_eq!(routes(&negative), Vec::<&str>::new());
}

#[test]
fn cobra_multiple_command_literals_do_not_collapse_into_one_statement() {
    let file = extract(
        "main.go",
        "package main\nfunc main() { cmds := []*cobra.Command{{Use: \"ignored\"}}; _ = cmds; a, b := &cobra.Command{Use: \"serve\"}, &cobra.Command{Use: \"migrate up\"}; _, _ = a, b }",
    );
    let names = routes(&file);
    assert!(names.contains(&"cmd serve"));
    assert!(names.contains(&"cmd migrate"));
    assert_eq!(
        routes(&extract(
            "other.go",
            "package main\nvar x = Other{Use: \"noise\"}"
        )),
        Vec::<&str>::new()
    );
}

#[test]
fn symfony_route_subdirectories_and_default_paths_require_handler_evidence() {
    for path in ["config/routes/admin.yaml", "config/routes_dev.yaml"] {
        let file = extract(
            path,
            "admin:\n  controller: App\\Controller\\AdminController::show\nempty:\n  description: omitted\n",
        );
        assert_eq!(routes(&file), ["admin"]);
        assert!(
            file.symbols
                .iter()
                .any(|symbol| symbol.body_search_text == "framework config route admin ANY /admin")
        );
    }
    // `<module>.routing.yml` is Drupal's convention, whose loader requires a
    // `path`; no route is synthesized for a path-less entry.
    assert_eq!(
        routes(&extract(
            "admin.routing.yml",
            "admin:\n  controller: App\\Controller\\AdminController::show\n"
        )),
        Vec::<&str>::new()
    );
    assert_eq!(
        routes(&extract(
            "config/settings.yaml",
            "admin:\n  path: /admin\n  controller: AdminController::show\n"
        )),
        Vec::<&str>::new()
    );
}

#[test]
fn codeigniter_404_and_argument_tails_do_not_become_controller_or_method_names() {
    let file = extract(
        "application/config/routes.php",
        "<?php $route['404_override'] = 'errors/page_missing';\n$route['shop'] = 'catalog/show/42';\n$route['admin/users'] = 'admin/users/show';\n$route['bad'] = 'catalog/$dynamic';",
    );
    assert!(routes(&file).contains(&"ANY <404>"));
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "catalog/show/42"
                && reference.resolution_name.as_deref() == Some("ci-route-root::Catalog::show"))
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "admin/users/show"
                && reference.resolution_name.as_deref() == Some("ci-route-path::admin/users/show"))
    );
    assert!(
        file.references
            .iter()
            .all(|reference| reference.resolution_name.as_deref() != Some("Show::42"))
    );
}

#[test]
fn codeigniter_model_loads_and_inferred_resources_keep_explicit_aliases_authoritative() {
    let file = extract(
        "application/models/User_model.php",
        "<?php class User_model extends CI_Model { function find() { $this->load->model('Audit_model'); $this->audit_model->log(); $this->other_model->find(); $this->Billing_lib->charge(); $this->db->get(); } }",
    );
    for lookup in [
        "ci-loaded::model::Audit_model",
        "ci-loaded::model::Audit_model::log",
        "ci-inferred::model::Other_model::find",
        "ci-inferred::library::Billing_lib::charge",
    ] {
        assert!(
            file.references
                .iter()
                .any(|reference| reference.owner.is_none()
                    && reference.resolution_name.as_deref() == Some(lookup)),
            "missing {lookup}"
        );
    }
    assert!(
        file.references
            .iter()
            .all(|reference| reference.resolution_name.as_deref()
                != Some("ci-inferred::library::Db::get"))
    );
    let conflict = extract(
        "application/models/X.php",
        "<?php class X extends CI_Model { function run() { $this->load->model('one', 'shared_model'); $this->load->model('two', 'shared_model'); $this->shared_model->save(); } }",
    );
    assert!(conflict.references.iter().all(|reference| {
        reference.kind != ReferenceKind::Calls
            || reference
                .resolution_name
                .as_deref()
                .is_none_or(|name| !name.contains("::save"))
    }));
}

#[test]
fn nested_builder_arrows_and_decorator_keywords_do_not_supply_route_arguments() {
    let file = extract(
        "lib/router.dart",
        "void setup() { final route = GoRoute(path: '/x', builder: (c, s) { final thunk = () => Wrong(); return Correct(); }); final app = MaterialApp(routes: {'/m': (_) { final text = '=> Wrong()'; return Correct(); }}); }",
    );
    assert_eq!(routes(&file), ["ANY /m", "ANY /x"]);
    for route in file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
    {
        assert!(
            file.references
                .iter()
                .all(|reference| reference.owner.as_ref() != Some(&route.id))
        );
    }
    let python = extract(
        "views.py",
        "@bp.route('/x', defaults=helper(methods=['POST']))\ndef x(): pass\n",
    );
    assert_eq!(routes(&python), ["ANY /x"]);
}

#[test]
fn codeigniter_dynamic_load_operands_and_quoted_load_text_do_not_bind_aliases() {
    for statement in [
        "$this->load->model($dynamic ?? 'audit_model'); $this->audit_model->log();",
        "$this->load->model('audit_model' . $suffix); $this->audit_model->log();",
        r#"$this->load->model("$dynamic"); $this->audit_model->log();"#,
        r#"$this->load->model('audit_model', "$dynamic"); $this->audit_model->log();"#,
        "$this->load->model('é_model'); $this->audit_model->log();",
        "$this->load->model('audit_model', 'é_alias'); $this->audit_model->log();",
        "$this->load->model('audit_model', $dynamic ?? 'audit_model'); $this->audit_model->log();",
        "$text = \"$this->load->model('audit_model', 'active')\"; $this->active->log();",
    ] {
        let source =
            format!("<?php class X extends CI_Model {{ function run() {{ {statement} }} }}");
        let file = extract("application/models/X.php", &source);
        assert!(
            file.references.iter().all(|reference| {
                reference
                    .resolution_name
                    .as_deref()
                    .is_none_or(|name| !name.contains("Audit_model"))
            }),
            "{statement}"
        );
    }
    let file = extract(
        "application/models/X.php",
        "<?php class X extends CI_Model { function run() { $this->load->library('audit', ['setting' => 'noise'], 'active'); $this->active->log(); } }",
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.resolution_name.as_deref()
                == Some("ci-loaded::library::Audit::log"))
    );
}

#[test]
fn aspnet_dynamic_attribute_expressions_cannot_become_literal_paths() {
    for attribute in [
        r#"[HttpGet(ROOT ?? "/fallback")]"#,
        r#"[HttpGet("/prefix" + ROOT)]"#,
        r#"[HttpGet($"/dynamic")]"#,
        r#"[HttpGet(Name = "label")]"#,
        r#"[Route("/prefix" + ROOT)][HttpGet("/child")]"#,
    ] {
        let source = format!("class OrdersController {{ {attribute} public void Index() {{}} }}");
        let file = extract("Orders.cs", &source);
        assert!(routes(&file).is_empty(), "{attribute}: {:?}", routes(&file));
    }
    let file = extract(
        "Orders.cs",
        r#"class OrdersController { [HttpGet(template: "/named", Name = "label")] public void Index() {} }"#,
    );
    assert_eq!(routes(&file), ["GET /named"]);
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "Index" && reference.kind == ReferenceKind::Calls)
    );
}

#[test]
fn codeigniter_backticks_cannot_seed_loads_or_resource_calls() {
    // Nowdoc/heredoc coverage remains unverified: the pinned PHP C scanner
    // crashes before enrichment on those forms.
    for literal in [
        "`$this->load->model('audit_model', 'active');`",
        "`\n$this->load->model('audit_model', 'active');\n`",
    ] {
        let source = format!(
            "<?php class X extends CI_Model {{ function run() {{ $text = {literal};\n$this->active->log(); }} }}"
        );
        let file = extract("application/models/X.php", &source);
        assert!(
            file.references.iter().all(|reference| reference
                .resolution_name
                .as_deref()
                .is_none_or(|name| !name.contains("Audit_model"))),
            "{literal}"
        );
    }
    for literal in ["`$this->active->log();`", "`\n$this->active->log();\n`"] {
        let source = format!(
            "<?php class X extends CI_Model {{ function run() {{ $this->load->model('audit_model', 'active'); $text = {literal};\n}} }}"
        );
        let file = extract("application/models/X.php", &source);
        assert!(
            file.references
                .iter()
                .all(|reference| reference.name != "log"),
            "{literal}"
        );
    }
    let file = extract(
        "application/models/X.php",
        "<?php class X extends CI_Model { function run() { $this->load->library('audit', `x, y`, 'active'); $this->active->log(); } }",
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.resolution_name.as_deref()
                == Some("ci-loaded::library::Audit::log"))
    );
}
