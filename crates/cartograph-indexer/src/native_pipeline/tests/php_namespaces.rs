//! PHP namespace, `use` import, include, and attribute-route resolution.
//!
//! PHP resolves class names at compile time from the namespace and the `use`
//! imports of the enclosing block; these generations prove the published edges
//! follow those rules exactly, abstain on vendor imports, and never fall back to
//! a same-named project class in another namespace.

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, DYNAMIC_DISPATCH_PROVENANCE,
    EXACT_SAME_FILE_PROVENANCE, EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE, EdgeKind, ReferenceInput,
    ReferenceKind, SymbolInput, build_capability_generation, capability_symbol,
};

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reversed = build_capability_generation(fixtures, true);
    assert_eq!(
        forward.digest(),
        reversed.digest(),
        "PHP resolution must not depend on file order"
    );
    forward
}

fn assert_targets(reference: &ReferenceInput, target: &SymbolInput) {
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{} must resolve to {}: {reference:?}",
        reference.reference_name,
        target.qualified_name
    );
}

const USER_MODEL: (&str, &str) = (
    "app/Models/User.php",
    "<?php\nnamespace App\\Models;\nclass User {\n    public static function find(int $id): ?self { return null; }\n    public static function active(): array { return []; }\n}\n",
);
const LEGACY_USER: (&str, &str) = (
    "app/Legacy/User.php",
    "<?php\nnamespace App\\Legacy;\nclass User {\n    public static function find(int $id) { return null; }\n    public static function active() { return []; }\n}\n",
);

#[test]
fn non_public_static_calls_require_class_scope_even_in_the_same_file() {
    let facts = generation(&[(
        "src/Visibility.php",
        r"<?php
class C {
    private static function hidden() {}
    protected static function guarded() {}
    public static function __callStatic($name, $args) {}
    public static function inside() { C::hidden(); C::guarded(); }
}
class Other { public static function run() { C::hidden(); C::guarded(); } }
function run() { C::hidden(); C::guarded(); }
class Plain { private static function hidden() {} }
function plain() { Plain::hidden(); }
",
    )]);
    for owner in ["run", "Other::run"] {
        let caller = capability_symbol(&facts, "src/Visibility.php", owner);
        for name in ["C::hidden", "C::guarded"] {
            let call =
                CapabilityReferenceQuery::new(&facts, caller).named(name, ReferenceKind::Calls);
            assert!(
                call.target_symbol_id.is_none(),
                "inaccessible {name}: {call:?}"
            );
        }
    }
    let plain = capability_symbol(&facts, "src/Visibility.php", "plain");
    assert!(
        CapabilityReferenceQuery::new(&facts, plain)
            .named("Plain::hidden", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    let inside = capability_symbol(&facts, "src/Visibility.php", "C::inside");
    for name in ["hidden", "guarded"] {
        assert_targets(
            CapabilityReferenceQuery::new(&facts, inside)
                .named(&format!("C::{name}"), ReferenceKind::Calls),
            capability_symbol(&facts, "src/Visibility.php", &format!("C::{name}")),
        );
    }
}

#[test]
fn protected_static_calls_require_proven_ancestry_across_files() {
    let facts = generation(&[
        (
            "src/Base.php",
            r"<?php
class Base {
    private static function hidden() {}
    protected static function guarded() {}
    public static function inside() { Base::hidden(); Base::guarded(); Child::childOnly(); }
}",
        ),
        (
            "src/Child.php",
            r"<?php
class Child extends Base {
    protected static function childOnly() {}
    public static function run() { Base::guarded(); Base::hidden(); }
}

class Grandchild extends Child { public static function run() { Base::guarded(); } }
class Other { public static function run() { Base::guarded(); } }
class Unknown extends Vendor { public static function run() { Base::guarded(); } }
function run() { Base::guarded(); }
",
        ),
    ]);
    let guarded = capability_symbol(&facts, "src/Base.php", "Base::guarded");
    for owner in ["Child::run", "Grandchild::run"] {
        let caller = capability_symbol(&facts, "src/Child.php", owner);
        assert_targets(
            CapabilityReferenceQuery::new(&facts, caller)
                .named("Base::guarded", ReferenceKind::Calls),
            guarded,
        );
    }
    for owner in ["Other::run", "Unknown::run", "run"] {
        let caller = capability_symbol(&facts, "src/Child.php", owner);
        assert!(
            CapabilityReferenceQuery::new(&facts, caller)
                .named("Base::guarded", ReferenceKind::Calls)
                .target_symbol_id
                .is_none()
        );
    }
    let child = capability_symbol(&facts, "src/Child.php", "Child::run");
    assert!(
        CapabilityReferenceQuery::new(&facts, child)
            .named("Base::hidden", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    let base = capability_symbol(&facts, "src/Base.php", "Base::inside");
    assert_targets(
        CapabilityReferenceQuery::new(&facts, base).named("Child::childOnly", ReferenceKind::Calls),
        capability_symbol(&facts, "src/Child.php", "Child::childOnly"),
    );
}

#[test]
fn private_calls_preserve_closure_scope_but_exclude_nested_named_functions() {
    let facts = generation(&[(
        "src/Scope.php",
        r"<?php
class C {
    private static function hidden() {}
    public static function inside() {
        $closure = function () { C::hidden(); };
        function nested() { C::hidden(); }
    }
}
",
    )]);
    let inside = capability_symbol(&facts, "src/Scope.php", "C::inside");
    assert_targets(
        CapabilityReferenceQuery::new(&facts, inside).named("C::hidden", ReferenceKind::Calls),
        capability_symbol(&facts, "src/Scope.php", "C::hidden"),
    );
    let nested = capability_symbol(&facts, "src/Scope.php", "nested");
    assert!(
        CapabilityReferenceQuery::new(&facts, nested)
            .named("C::hidden", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn use_imports_bind_static_calls_constructors_and_declarations_to_the_imported_namespace() {
    let facts = generation(&[
        USER_MODEL,
        LEGACY_USER,
        (
            "app/Models/Post.php",
            "<?php\nnamespace App\\Models;\nclass Post { public static function all(): array { return []; } }\n",
        ),
        (
            "app/Http/Controllers/OrderController.php",
            r"<?php
namespace App\Http\Controllers;

use App\Models\User;
use App\Models\{Post as P};

class OrderController
{
    public function index(): void
    {
        User::find(1);
        user::active();
        P::all();
        new User();
        User::where('a', 1);
    }
}
",
        ),
    ]);
    let index = capability_symbol(
        &facts,
        "app/Http/Controllers/OrderController.php",
        r"App\Http\Controllers::OrderController::index",
    );
    let user = capability_symbol(&facts, "app/Models/User.php", r"App\Models::User");
    let find = capability_symbol(&facts, "app/Models/User.php", r"App\Models::User::find");
    let active = capability_symbol(&facts, "app/Models/User.php", r"App\Models::User::active");
    let all = capability_symbol(&facts, "app/Models/Post.php", r"App\Models::Post::all");

    assert_targets(
        CapabilityReferenceQuery::new(&facts, index).named("User::find", ReferenceKind::Calls),
        find,
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, index).named("user::active", ReferenceKind::Calls),
        active,
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, index).named("P::all", ReferenceKind::Calls),
        all,
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, index).named("User", ReferenceKind::Instantiates),
        user,
    );
    assert!(facts.edges().iter().any(|edge| {
        edge.source_symbol_id == index.symbol_id
            && edge.target_symbol_id == user.symbol_id
            && edge.kind == EdgeKind::Instantiates
    }));
    let inherited =
        CapabilityReferenceQuery::new(&facts, index).named("User::where", ReferenceKind::Calls);
    assert!(
        inherited.target_symbol_id.is_none(),
        "an undeclared static member is not guessed onto the class: {inherited:?}"
    );

    let declaration = facts
        .references()
        .iter()
        .find(|reference| {
            reference.reference_kind == ReferenceKind::References.as_str()
                && reference.reference_name == "User"
                && reference.target_symbol_id.is_some()
        })
        .unwrap_or_else(|| panic!("the use clause must reference the imported class"));
    assert_eq!(declaration.target_symbol_id.as_ref(), Some(&user.symbol_id));
}

#[test]
fn namespace_relative_names_and_fully_qualified_names_resolve_without_imports() {
    let facts = generation(&[
        USER_MODEL,
        LEGACY_USER,
        (
            "app/Legacy/Importer.php",
            r"<?php
namespace App\Legacy;

class Importer extends Base
{
    public function run(): void
    {
        User::find(1);
        \App\Models\User::active();
        parent::boot();
        self::helper();
        $this->helper();
    }

    private static function helper(): void {}
}
",
        ),
        (
            "app/Legacy/Base.php",
            "<?php\nnamespace App\\Legacy;\nabstract class Base { public static function boot(): void {} }\n",
        ),
    ]);
    let run = capability_symbol(
        &facts,
        "app/Legacy/Importer.php",
        r"App\Legacy::Importer::run",
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("User::find", ReferenceKind::Calls),
        capability_symbol(&facts, "app/Legacy/User.php", r"App\Legacy::User::find"),
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run)
            .named(r"\App\Models\User::active", ReferenceKind::Calls),
        capability_symbol(&facts, "app/Models/User.php", r"App\Models::User::active"),
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("parent::boot", ReferenceKind::Calls),
        capability_symbol(&facts, "app/Legacy/Base.php", r"App\Legacy::Base::boot"),
    );
    let helper = capability_symbol(
        &facts,
        "app/Legacy/Importer.php",
        r"App\Legacy::Importer::helper",
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("self::helper", ReferenceKind::Calls),
        helper,
    );
    let exact =
        CapabilityReferenceQuery::new(&facts, run).named("self::helper", ReferenceKind::Calls);
    assert_eq!(exact.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
    let dispatched =
        CapabilityReferenceQuery::new(&facts, run).named("$this->helper", ReferenceKind::Calls);
    assert_targets(dispatched, helper);
    assert_eq!(
        dispatched.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE,
        "an override can receive a $this call, so the edge is dispatch, not exact"
    );
    let importer = capability_symbol(&facts, "app/Legacy/Importer.php", r"App\Legacy::Importer");
    assert_targets(
        CapabilityReferenceQuery::new(&facts, importer).named("Base", ReferenceKind::Extends),
        capability_symbol(&facts, "app/Legacy/Base.php", r"App\Legacy::Base"),
    );
}

#[test]
fn vendor_imports_stay_external_instead_of_falling_back_to_project_names() {
    let facts = generation(&[
        (
            "app/Support/Cache.php",
            "<?php\nnamespace App\\Support;\nclass Cache { public static function get(string $key) { return null; } }\n",
        ),
        (
            "app/Http/Controllers/HomeController.php",
            r"<?php
namespace App\Http\Controllers;

use Illuminate\Support\Facades\Cache;

class HomeController
{
    public function show(): void
    {
        Cache::get('k');
    }
}
",
        ),
    ]);
    let show = capability_symbol(
        &facts,
        "app/Http/Controllers/HomeController.php",
        r"App\Http\Controllers::HomeController::show",
    );
    let call =
        CapabilityReferenceQuery::new(&facts, show).named("Cache::get", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    assert_eq!(
        call.resolution_provenance,
        EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
    );
}

#[test]
fn function_and_global_imports_resolve_to_their_exact_declarations() {
    let facts = generation(&[
        (
            "src/Support/helpers.php",
            "<?php\nnamespace App\\Support;\nfunction format_money(int $cents): string { return ''; }\n",
        ),
        (
            "src/Other/helpers.php",
            "<?php\nnamespace App\\Other;\nfunction format_money(int $cents): string { return ''; }\n",
        ),
        (
            "src/Legacy.php",
            "<?php\nclass Legacy { public static function run(): void {} }\n",
        ),
        (
            "src/Service/Billing.php",
            r"<?php
namespace App\Service;

use function App\Support\format_money;
use Legacy;

function bill(): void
{
    format_money(1);
    Legacy::run();
}
",
        ),
    ]);
    let bill = capability_symbol(&facts, "src/Service/Billing.php", r"App\Service::bill");
    assert_targets(
        CapabilityReferenceQuery::new(&facts, bill).named("format_money", ReferenceKind::Calls),
        capability_symbol(
            &facts,
            "src/Support/helpers.php",
            r"App\Support::format_money",
        ),
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, bill).named("Legacy::run", ReferenceKind::Calls),
        capability_symbol(&facts, "src/Legacy.php", "Legacy::run"),
    );
}

#[test]
fn literal_include_once_and_require_once_targets_resolve_to_project_files() {
    let facts = generation(&[
        ("lib/helpers.php", "<?php\nfunction lib_helper() {}\n"),
        (
            "public/partials/header.php",
            "<?php\nfunction header_partial() {}\n",
        ),
        (
            "public/index.php",
            "<?php\ninclude_once('partials/header.php');\nrequire_once 'lib/helpers.php';\nrequire_once __DIR__ . '/missing.php';\nlib_helper();\n",
        ),
    ]);
    let index_file = facts
        .files()
        .iter()
        .find(|file| file.normalized_path == "public/index.php")
        .unwrap_or_else(|| panic!("missing index file"));
    for (name, target_path) in [
        ("partials/header.php", "public/partials/header.php"),
        ("lib/helpers.php", "lib/helpers.php"),
    ] {
        let target = facts
            .files()
            .iter()
            .find(|file| file.normalized_path == target_path)
            .unwrap_or_else(|| panic!("missing include target {target_path}"));
        let include = facts
            .references()
            .iter()
            .find(|reference| {
                reference.file_id == index_file.file_id
                    && reference.reference_kind == ReferenceKind::Imports.as_str()
                    && reference.reference_name == name
            })
            .unwrap_or_else(|| panic!("missing include {name}: {:?}", facts.references()));
        let target_symbol = facts
            .symbols()
            .iter()
            .find(|symbol| symbol.file_id == target.file_id && symbol.symbol_kind == "file")
            .unwrap_or_else(|| panic!("missing file symbol for {target_path}"));
        assert_eq!(
            include.target_symbol_id.as_ref(),
            Some(&target_symbol.symbol_id),
            "{include:?}"
        );
    }
    assert!(
        facts
            .references()
            .iter()
            .filter(|reference| reference.file_id == index_file.file_id)
            .all(|reference| !reference.reference_name.contains("missing")),
        "a computed include path is not an import"
    );
    let call = facts
        .references()
        .iter()
        .find(|reference| {
            reference.file_id == index_file.file_id && reference.reference_name == "lib_helper"
        })
        .unwrap_or_else(|| panic!("missing lib_helper call"));
    assert!(
        call.target_symbol_id.is_some(),
        "an include binding must not block the project-wide name fallback: {call:?}"
    );
}

#[test]
fn symfony_attribute_routes_call_their_controller_actions() {
    let facts = generation(&[(
        "src/Controller/BlogController.php",
        r"<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route;

#[Route('/blog', name: 'blog_')]
class BlogController
{
    #[Route('/list', name: 'list', methods: ['GET'])]
    public function list(): void {}
}
",
    )]);
    let route = facts
        .symbols()
        .iter()
        .find(|symbol| symbol.symbol_kind == "route" && symbol.qualified_name.contains("blog_list"))
        .unwrap_or_else(|| panic!("missing Symfony route: {:?}", facts.symbols()));
    assert_targets(
        CapabilityReferenceQuery::new(&facts, route).named("list", ReferenceKind::Calls),
        capability_symbol(
            &facts,
            "src/Controller/BlogController.php",
            r"App\Controller::BlogController::list",
        ),
    );
}

#[test]
fn exact_php_lookups_respect_symbol_spaces_case_and_global_names() {
    let facts = generation(&[
        (
            "src/User.php",
            "<?php\nnamespace App;\nclass User { public static function make() {} }\n",
        ),
        (
            "src/Other/Widget.php",
            "<?php\nnamespace Other;\nclass Widget {}\nfunction helper() {}\n",
        ),
        (
            "src/global.php",
            "<?php\nfunction helper() {}\nfunction shout() {}\n",
        ),
        (
            "src/Base.php",
            "<?php\nnamespace App;\nabstract class Base { protected static function boot() {} }\n",
        ),
        (
            "src/Consumer.php",
            r"<?php
namespace App;

use Vendor\Thing as T;
use Vendor\User as V;

function T() {}
function shout() {}

class Consumer extends Base
{
    public function run(): void
    {
        new T();
        T();
        new user();
        new \Widget();
        helper();
        shout();
        parent::boot();
    }
}
",
        ),
    ]);
    let run = capability_symbol(&facts, "src/Consumer.php", r"App::Consumer::run");
    let construction =
        CapabilityReferenceQuery::new(&facts, run).named("T", ReferenceKind::Instantiates);
    assert!(
        construction.target_symbol_id.is_none(),
        "a class import never binds to a same-named function: {construction:?}"
    );
    assert_eq!(
        construction.resolution_provenance,
        EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("T", ReferenceKind::Calls),
        capability_symbol(&facts, "src/Consumer.php", "App::T"),
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("user", ReferenceKind::Instantiates),
        capability_symbol(&facts, "src/User.php", "App::User"),
    );
    let global =
        CapabilityReferenceQuery::new(&facts, run).named(r"\Widget", ReferenceKind::Instantiates);
    assert!(
        global.target_symbol_id.is_none(),
        "a global class name never binds to a namespaced one: {global:?}"
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("helper", ReferenceKind::Calls),
        capability_symbol(&facts, "src/global.php", "helper"),
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("shout", ReferenceKind::Calls),
        capability_symbol(&facts, "src/Consumer.php", "App::shout"),
    );
    assert_targets(
        CapabilityReferenceQuery::new(&facts, run).named("parent::boot", ReferenceKind::Calls),
        capability_symbol(&facts, "src/Base.php", "App::Base::boot"),
    );

    let vendor_alias = facts
        .references()
        .iter()
        .find(|reference| {
            reference.reference_kind == ReferenceKind::References.as_str()
                && reference.reference_name == "User"
        })
        .unwrap_or_else(|| panic!("missing aliased vendor import declaration"));
    assert!(
        vendor_alias.target_symbol_id.is_none(),
        "an aliased vendor import must not bind to the project's App\\User: {vendor_alias:?}"
    );
}

/// Provenance of an undeclared static member bound to its Eloquent model class.
const ELOQUENT_MODEL_PROVENANCE: &str = "framework-laravel-eloquent-model";

/// Eloquent models by direct, aliased, fully qualified, and indirect ancestry, models whose
/// project traits supply members, plus non-models.
const ELOQUENT_FIXTURES: [(&str, &str); 8] = [
    (
        "app/Models/Tagged.php",
        r"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;
use Illuminate\Database\Eloquent\Factories\HasFactory;

trait LocalQueries { public static function search(): array { return []; } }
trait Outer { use LocalQueries; }
class Tagged extends Model { use HasFactory; use LocalQueries; }
class Nested extends Model { use Outer; }
class Aliased extends Model { use LocalQueries { search as where; } }
",
    ),
    (
        "app/Models/User.php",
        r"<?php
namespace App\Models;

use Illuminate\Foundation\Auth\User as Authenticatable;

class User extends Authenticatable
{
public static function active(): array { return []; }
}
",
    ),
    (
        "app/LegacyOrder.php",
        "<?php\nnamespace App;\nclass LegacyOrder extends \\Illuminate\\Database\\Eloquent\\Model {}\n",
    ),
    (
        "app/Models/BaseModel.php",
        "<?php\nnamespace App\\Models;\nuse Illuminate\\Database\\Eloquent\\Model;\nabstract class BaseModel extends Model { public static function report(): array { return []; } }\n",
    ),
    (
        "app/Models/Post.php",
        "<?php\nnamespace App\\Models;\nclass Post extends BaseModel {}\n",
    ),
    (
        "app/Services/Billing.php",
        "<?php\nnamespace App\\Services;\nclass Billing extends \\Vendor\\Service {}\n",
    ),
    (
        "app/Services/Loop.php",
        "<?php\nnamespace App\\Services;\nclass LoopA extends LoopB {}\nclass LoopB extends LoopA {}\n",
    ),
    (
        "app/Http/Controllers/OrderController.php",
        r"<?php
namespace App\Http\Controllers;

use App\Models\User;
use App\Models\Post;
use App\LegacyOrder;
use App\Services\Billing;
use App\Services\LoopA;
use App\Models\Tagged;
use App\Models\Nested;
use App\Models\Aliased;
use Illuminate\Support\Facades\Cache;

class OrderController
{
public function index(): void
{
    User::active();
    User::where('a', 1);
    LegacyOrder::query();
    Post::where('b', 2);
    Post::report();
    Billing::where('c', 3);
    LoopA::where('d', 4);
    Cache::get('k');
    Tagged::where('t', 1);
    Tagged::factory();
    Tagged::search();
    Nested::search();
    Nested::where('n', 1);
    Aliased::where('x', 1);
}
}
",
    ),
];

#[test]
fn undeclared_static_members_of_eloquent_models_bind_to_the_model_class() {
    let facts = generation(&ELOQUENT_FIXTURES);
    let index = capability_symbol(
        &facts,
        "app/Http/Controllers/OrderController.php",
        r"App\Http\Controllers::OrderController::index",
    );
    let call =
        |name: &str| CapabilityReferenceQuery::new(&facts, index).named(name, ReferenceKind::Calls);
    let declared = call("User::active");
    assert_targets(
        declared,
        capability_symbol(&facts, "app/Models/User.php", r"App\Models::User::active"),
    );
    assert_ne!(
        declared.resolution_provenance, ELOQUENT_MODEL_PROVENANCE,
        "a declared member resolves exactly, not through the model fallback"
    );
    for (name, path, model) in [
        ("User::where", "app/Models/User.php", r"App\Models::User"),
        (
            "LegacyOrder::query",
            "app/LegacyOrder.php",
            r"App::LegacyOrder",
        ),
        ("Post::where", "app/Models/Post.php", r"App\Models::Post"),
        (
            "Tagged::where",
            "app/Models/Tagged.php",
            r"App\Models::Tagged",
        ),
        (
            "Tagged::factory",
            "app/Models/Tagged.php",
            r"App\Models::Tagged",
        ),
        (
            "Nested::where",
            "app/Models/Tagged.php",
            r"App\Models::Nested",
        ),
    ] {
        let reference = call(name);
        assert_targets(reference, capability_symbol(&facts, path, model));
        assert_eq!(
            reference.resolution_provenance, ELOQUENT_MODEL_PROVENANCE,
            "{reference:?}"
        );
        assert!(reference.confidence < 0.9, "{reference:?}");
    }
    for (name, path, method) in [
        (
            "Post::report",
            "app/Models/BaseModel.php",
            r"App\Models::BaseModel::report",
        ),
        (
            "Nested::search",
            "app/Models/Tagged.php",
            r"App\Models::LocalQueries::search",
        ),
    ] {
        let supplied = call(name);
        assert_targets(supplied, capability_symbol(&facts, path, method));
        assert_eq!(
            supplied.resolution_provenance,
            super::EXACT_PROJECT_PROVENANCE
        );
    }
    assert!(
        call("Tagged::search").target_symbol_id.is_none(),
        "the external HasFactory trait may conflict"
    );
    assert!(
        call("Aliased::where").target_symbol_id.is_none(),
        "trait adaptations remain unknown"
    );
    let aliased = capability_symbol(&facts, "app/Models/Tagged.php", r"App\Models::Aliased");
    assert_targets(
        CapabilityReferenceQuery::new(&facts, aliased)
            .named("LocalQueries", ReferenceKind::Implements),
        capability_symbol(&facts, "app/Models/Tagged.php", r"App\Models::LocalQueries"),
    );
    for name in ["Billing::where", "LoopA::where", "Cache::get"] {
        let reference = call(name);
        assert!(
            reference.target_symbol_id.is_none(),
            "{name} has no Eloquent model ancestry and must stay unresolved: {reference:?}"
        );
    }
}

/// Factories whose declared return types do and do not name one receiver class.
const FACTORY_FIXTURES: [(&str, &str); 5] = [
    (
        "src/Receiver.php",
        "<?php\nclass Receiver { public function send(): void {} }\nclass Factory { public static function make(): Receiver { return new Receiver(); } }\n",
    ),
    ("src/receiver_upper.php", "<?php\nclass RECEIVER {}\n"),
    (
        "ApiClient.php",
        r"<?php
class ApiClient {
  public static function for(string $credential): ?self { return new self(); }
  public static function make(): static { return new static(); }
  public static function named(): ApiClient { return new ApiClient(); }
  public static function other(): Other { return new Other(); }
  public static function either(): ApiClient|Other { return new Other(); }
  public static function untyped() { return new self(); }
  private static function hidden(): self { return new self(); }
  public function createOrder(): void {}
  private function secret(): void {}
  public function rebuild(): void { self::hidden()->createOrder(); self::make()->secret(); }
}
class Other { public function createOrder(): void {} }
trait Builds {
  public static function build(): static { return new static(); }
  public function createOrder(): void {}
  public function other(): Other { return new Other(); }
  public function chain(): void { $this->other()->createOrder(); }
}
function run(): void {
  ApiClient::for('cred-123')->createOrder();
  ApiClient::make()->createOrder();
  ApiClient::named()->createOrder();
  ApiClient::other()->createOrder();
  ApiClient::either()->createOrder();
  ApiClient::untyped()->createOrder();
  ApiClient::missing()->createOrder();
  Builds::build()->createOrder();
  ApiClient::hidden()->createOrder();
  ApiClient::make()->secret();
  Factory::make()->send();
}
",
    ),
    (
        "src/Shop/Checkout.php",
        r"<?php
namespace App\Shop;

use App\Api\Client;

class Checkout
{
public function pay(): void
{
    Client::connect()?->send();
    $this->client()->send();
    Client::hidden()->send();
}

private function client(): Client { return Client::connect(); }
}
",
    ),
    (
        "src/Api/Client.php",
        "<?php\nnamespace App\\Api;\nfinal class Client {\n  public static function connect(): ?static { return null; }\n  protected static function hidden(): static { return new static(); }\n  public function send(): void {}\n}\n",
    ),
];

#[test]
fn static_factory_chains_call_the_method_of_the_declared_return_type() {
    let facts = generation(&FACTORY_FIXTURES);
    let run = capability_symbol(&facts, "ApiClient.php", "run");
    let create = capability_symbol(&facts, "ApiClient.php", "ApiClient::createOrder");
    let other_create = capability_symbol(&facts, "ApiClient.php", "Other::createOrder");
    let call = |owner, name: &str| {
        CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls)
    };
    for (name, target) in [
        ("ApiClient::for()->createOrder", create),
        ("ApiClient::make()->createOrder", create),
        ("ApiClient::named()->createOrder", create),
        ("ApiClient::other()->createOrder", other_create),
    ] {
        let reference = call(run, name);
        assert_targets(reference, target);
        assert_eq!(
            reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE,
            "the returned object may be a subclass, so the edge is dispatch: {reference:?}"
        );
    }
    assert!(facts.edges().iter().any(|edge| {
        edge.source_symbol_id == run.symbol_id
            && edge.target_symbol_id == create.symbol_id
            && edge.kind == EdgeKind::Calls
    }));
    for name in [
        "ApiClient::either()->createOrder",
        "ApiClient::untyped()->createOrder",
        "ApiClient::missing()->createOrder",
        "Builds::build()->createOrder",
        "Factory::make()->send",
    ] {
        let reference = call(run, name);
        assert!(
            reference.target_symbol_id.is_none(),
            "{name} has no single declared receiver class (static in a trait is the using class): {reference:?}"
        );
    }
    for name in [
        "ApiClient::hidden()->createOrder",
        "ApiClient::make()->secret",
    ] {
        let private = call(run, name);
        assert!(
            private.target_symbol_id.is_none(),
            "outside its class a private call can reach __callStatic or __call instead: {private:?}"
        );
    }
    let rebuild = capability_symbol(&facts, "ApiClient.php", "ApiClient::rebuild");
    assert_targets(call(rebuild, "self::hidden()->createOrder"), create);
    assert_targets(
        call(rebuild, "self::make()->secret"),
        capability_symbol(&facts, "ApiClient.php", "ApiClient::secret"),
    );
    let chain = capability_symbol(&facts, "ApiClient.php", "Builds::chain");
    let in_trait = call(chain, "$this->other()->createOrder");
    assert!(
        in_trait.target_symbol_id.is_none(),
        "the class using a trait can replace the trait's factory: {in_trait:?}"
    );
    let pay = capability_symbol(&facts, "src/Shop/Checkout.php", r"App\Shop::Checkout::pay");
    let send = capability_symbol(&facts, "src/Api/Client.php", r"App\Api::Client::send");
    assert_targets(call(pay, "Client::connect()?->send"), send);
    assert_targets(call(pay, "$this->client()->send"), send);
    let hidden = call(pay, "Client::hidden()->send");
    assert!(
        hidden.target_symbol_id.is_none(),
        "a protected factory called from an unrelated file is not followed: {hidden:?}"
    );
}

#[test]
fn php_class_function_and_method_names_match_case_insensitively_and_constants_do_not() {
    let facts = generation(&[
        ("src/g.php", "<?php\nfunction ping() {}\n"),
        (
            "src/n.php",
            r"<?php
namespace N;

use const App\Config\LIMIT;
use const app\CONFIG\MAX;

function Ping() {}

class Maker
{
    public static function Make(): void {}

    public function run(): void
    {
        ping();
        self::make();
        new maker();
        \n\MAKER::MAKE();
    }
}
",
        ),
        (
            "src/config.php",
            "<?php\nnamespace App\\Config;\nconst limit = 1;\nconst MAX = 2;\n",
        ),
        (
            "src/dupes.php",
            "<?php\nnamespace D;\nclass Twin {}\nfunction call(): void { new twin(); }\n",
        ),
        ("src/dupes2.php", "<?php\nnamespace D;\nclass TWIN {}\n"),
    ]);
    let run = capability_symbol(&facts, "src/n.php", r"N::Maker::run");
    let call = |name: &str, kind| CapabilityReferenceQuery::new(&facts, run).named(name, kind);
    let ping = call("ping", ReferenceKind::Calls);
    assert_targets(ping, capability_symbol(&facts, "src/n.php", r"N::Ping"));
    let make = capability_symbol(&facts, "src/n.php", r"N::Maker::Make");
    assert_targets(call("self::make", ReferenceKind::Calls), make);
    assert_targets(call(r"\n\MAKER::MAKE", ReferenceKind::Calls), make);
    assert_targets(
        call("maker", ReferenceKind::Instantiates),
        capability_symbol(&facts, "src/n.php", r"N::Maker"),
    );
    let constant = facts
        .references()
        .iter()
        .find(|reference| {
            reference.reference_kind == ReferenceKind::References.as_str()
                && reference.reference_name == "LIMIT"
        })
        .unwrap_or_else(|| panic!("missing const import declaration"));
    assert!(
        constant.target_symbol_id.is_none(),
        "PHP constant names are case-sensitive: {constant:?}"
    );
    let maximum = facts
        .references()
        .iter()
        .find(|reference| {
            reference.reference_kind == ReferenceKind::References.as_str()
                && reference.reference_name == "MAX"
        })
        .unwrap_or_else(|| panic!("missing const import declaration"));
    assert_targets(
        maximum,
        capability_symbol(&facts, "src/config.php", r"App\Config::MAX"),
    );
    let twin_call = capability_symbol(&facts, "src/dupes.php", r"D::call");
    let twin =
        CapabilityReferenceQuery::new(&facts, twin_call).named("twin", ReferenceKind::Instantiates);
    assert!(
        twin.target_symbol_id.is_none(),
        "two declarations that differ only in case are ambiguous: {twin:?}"
    );
}
