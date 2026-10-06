//! PHP extraction contracts restored from the v1 extractor's acceptance scenarios.
//!
//! The v1 TypeScript runtime extracted `use` imports, literal includes, inheritance, traits, enum
//! cases, member/static calls, type references, and property names for PHP. These tests pin the
//! v2-native shape of each behavior, including the cases that must stay unextracted.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedReference, ExtractedSymbol, ImportBindingKind,
    NativeExtractor, PHP_EXACT_RESOLUTION_PREFIX, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::Php, "{path}");
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    let file = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"));
    assert_eq!(file.parse_status, FileParseStatus::Parsed, "{path}");
    file
}

fn symbol<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind:?} {name}: {:#?}", file.symbols))
}

fn reference<'file>(
    file: &'file ExtractedFile,
    kind: ReferenceKind,
    name: &str,
) -> &'file ExtractedReference {
    file.references
        .iter()
        .find(|reference| reference.kind == kind && reference.name == name)
        .unwrap_or_else(|| panic!("missing {kind:?} {name}: {:#?}", file.references))
}

/// The exact compile-time lookup the extractor records for a PHP name.
fn lookup(intent: &str, key: &str) -> String {
    format!("{PHP_EXACT_RESOLUTION_PREFIX}{intent}::{key}")
}

fn names_of(file: &ExtractedFile, kind: SymbolKind) -> Vec<&str> {
    let mut names = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

fn reference_names(file: &ExtractedFile, kind: ReferenceKind) -> Vec<&str> {
    let mut names = file
        .references
        .iter()
        .filter(|reference| reference.kind == kind)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

fn owned_reference_names(
    file: &ExtractedFile,
    owner: &ExtractedSymbol,
    kind: ReferenceKind,
) -> Vec<String> {
    let mut names = file
        .references
        .iter()
        .filter(|reference| reference.owner.as_ref() == Some(&owner.id) && reference.kind == kind)
        .map(|reference| reference.name.clone())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

#[test]
fn simple_and_aliased_use_clauses_bind_their_local_names() {
    let simple = extract("Test.php", r"<?php use PHPUnit\Framework\TestCase;");
    assert_eq!(
        names_of(&simple, SymbolKind::Import),
        [r"PHPUnit\Framework\TestCase"]
    );
    assert_eq!(
        reference_names(&simple, ReferenceKind::Imports),
        [r"PHPUnit\Framework\TestCase"]
    );
    let [binding] = simple.import_bindings.as_slice() else {
        panic!(
            "simple use must bind exactly once: {:?}",
            simple.import_bindings
        );
    };
    assert_eq!(binding.kind, ImportBindingKind::Named);
    assert_eq!(binding.module_specifier, r"PHPUnit\Framework");
    assert_eq!(binding.imported_name, "TestCase");
    assert_eq!(binding.local_name, "TestCase");

    let aliased = extract("Test.php", "<?php use Mockery as m;");
    let import = symbol(&aliased, SymbolKind::Import, "Mockery");
    assert!(
        import
            .signature
            .as_deref()
            .is_some_and(|signature| signature.contains("as m")),
        "{import:?}"
    );
    assert!(
        names_of(&aliased, SymbolKind::Module).is_empty(),
        "an aliased use is not a module declaration: {:?}",
        aliased.symbols
    );
    let [binding] = aliased.import_bindings.as_slice() else {
        panic!(
            "aliased use must bind exactly once: {:?}",
            aliased.import_bindings
        );
    };
    assert_eq!(binding.module_specifier, "\\");
    assert_eq!(binding.imported_name, "Mockery");
    assert_eq!(binding.local_name, "m");
}

#[test]
fn function_grouped_and_multiple_use_clauses_expand_to_one_import_each() {
    let function = extract("helpers.php", r"<?php use function Illuminate\Support\env;");
    let import = symbol(&function, SymbolKind::Import, r"Illuminate\Support\env");
    assert!(
        import
            .signature
            .as_deref()
            .is_some_and(|signature| signature.contains("function")),
        "{import:?}"
    );

    let grouped = extract(
        "Models.php",
        r"<?php use Illuminate\Database\{Model, Builder as QueryBuilder};",
    );
    assert_eq!(
        names_of(&grouped, SymbolKind::Import),
        [r"Illuminate\Database\Builder", r"Illuminate\Database\Model"]
    );
    let mut locals = grouped
        .import_bindings
        .iter()
        .map(|binding| {
            (
                binding.module_specifier.as_str(),
                binding.imported_name.as_str(),
                binding.local_name.as_str(),
            )
        })
        .collect::<Vec<_>>();
    locals.sort_unstable();
    assert_eq!(
        locals,
        [
            (r"Illuminate\Database", "Builder", "QueryBuilder"),
            (r"Illuminate\Database", "Model", "Model"),
        ]
    );

    let multiple = extract(
        "Service.php",
        "<?php\nuse Illuminate\\Support\\Collection;\nuse Illuminate\\Support\\Str;\nuse Closure;\n",
    );
    assert_eq!(
        names_of(&multiple, SymbolKind::Import),
        [
            "Closure",
            r"Illuminate\Support\Collection",
            r"Illuminate\Support\Str"
        ]
    );
    for import in multiple
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Import)
    {
        assert!(
            !multiple
                .containments
                .iter()
                .any(|containment| containment.child == import.id),
            "imports stay at file scope: {import:?}"
        );
    }
    let declaration_site = reference(&multiple, ReferenceKind::References, "Collection");
    assert!(declaration_site.owner.is_none());
    assert!(multiple.import_bindings.iter().any(|binding| {
        binding.local_name == "Collection" && binding.span == declaration_site.span
    }));
}

#[test]
fn literal_includes_are_file_imports_and_dynamic_targets_are_skipped() {
    let source = "<?php\ninclude 'lib/bootstrap.php';\ninclude_once(\"partials/header.php\");\nrequire \"vendor/autoload.php\";\nrequire_once('config/app.php');\n";
    let file = extract("index.php", source);
    let expected = [
        "config/app.php",
        "lib/bootstrap.php",
        "partials/header.php",
        "vendor/autoload.php",
    ];
    assert_eq!(names_of(&file, SymbolKind::Import), expected);
    assert_eq!(reference_names(&file, ReferenceKind::Imports), expected);
    for path in expected {
        let include = reference(&file, ReferenceKind::Imports, path);
        assert!(include.owner.is_none(), "{include:?}");
        assert!(
            file.import_bindings.iter().any(|binding| {
                binding.kind == ImportBindingKind::IncludeQuoted
                    && binding.module_specifier == path
                    && binding.span == include.span
            }),
            "include {path} must bind as a quoted file include: {:?}",
            file.import_bindings
        );
    }

    let dynamic = extract(
        "dynamic.php",
        "<?php\ninclude $template;\ninclude_once(\"views/$name.php\");\nrequire_once __DIR__ . '/config.php';\nrequire '';\n",
    );
    assert!(
        names_of(&dynamic, SymbolKind::Import).is_empty(),
        "{:?}",
        dynamic.symbols
    );
    assert!(
        reference_names(&dynamic, ReferenceKind::Imports).is_empty(),
        "{:?}",
        dynamic.references
    );
    assert!(
        dynamic.import_bindings.is_empty(),
        "{:?}",
        dynamic.import_bindings
    );
}

#[test]
fn class_inheritance_emits_extends_and_every_implemented_interface() {
    let file = extract(
        "ChildController.php",
        "<?php\n\nclass ChildController extends BaseController implements Serializable, JsonSerializable\n{\n    public function serialize(): string\n    {\n        return json_encode($this);\n    }\n}\n",
    );
    let class = symbol(&file, SymbolKind::Class, "ChildController");
    assert_eq!(
        owned_reference_names(&file, class, ReferenceKind::Extends),
        ["BaseController"]
    );
    assert_eq!(
        owned_reference_names(&file, class, ReferenceKind::Implements),
        ["JsonSerializable", "Serializable"]
    );
    assert!(
        reference_names(&file, ReferenceKind::Inherits).is_empty(),
        "PHP inheritance is explicit, never the generic inherits shape: {:?}",
        file.references
    );

    let interfaces = extract(
        "src/Contracts.php",
        "<?php\nnamespace App\\Contracts;\ninterface Repository extends Countable, \\IteratorAggregate {}\n",
    );
    let interface = symbol(&interfaces, SymbolKind::Interface, "Repository");
    assert_eq!(
        owned_reference_names(&interfaces, interface, ReferenceKind::Extends),
        ["Countable", "\\IteratorAggregate"]
    );
    let global = reference(&interfaces, ReferenceKind::Extends, "\\IteratorAggregate");
    assert_eq!(
        global.resolution_name,
        Some(lookup("class", "IteratorAggregate"))
    );
    let local = reference(&interfaces, ReferenceKind::Extends, "Countable");
    assert_eq!(
        local.resolution_name,
        Some(lookup("class", "App\\Contracts::Countable"))
    );
}

const DECK_SOURCE: &str = r"<?php
namespace App\Cards;

trait Loggable { public function log(): void {} }

enum Suit: string implements HasLabel {
    case Hearts = 'H';
    case Spades = 'S';
    const Wild = self::Spades;
}

final class Deck {
    use Loggable, \Other\Trait2;
    const MAX = 10, MIN = 1;
    private Repo $repo;
    public static ?int $count = 0;
    protected $untyped;
    var $legacy;

    function shuffle() {}
    private static function seed(): int { return 1; }
}
";

#[test]
fn traits_enum_cases_and_constants_keep_php_declaration_kinds() {
    let file = extract("src/Deck.php", DECK_SOURCE);
    let deck = symbol(&file, SymbolKind::Class, "Deck");
    assert_eq!(deck.qualified_name, r"App\Cards::Deck");
    assert!(!deck.execution.static_member, "a final class is not static");
    assert!(deck.export.exported);
    assert_eq!(
        symbol(&file, SymbolKind::Namespace, r"App\Cards").qualified_name,
        r"App\Cards"
    );

    let trait_symbol = symbol(&file, SymbolKind::Trait, "Loggable");
    assert_eq!(trait_symbol.qualified_name, r"App\Cards::Loggable");
    assert!(
        names_of(&file, SymbolKind::Interface).is_empty(),
        "{:?}",
        file.symbols
    );
    assert_eq!(
        owned_reference_names(&file, deck, ReferenceKind::Implements),
        ["Loggable", r"\Other\Trait2"]
    );
    assert!(
        reference_names(&file, ReferenceKind::Imports).is_empty(),
        "trait use is not a file import: {:?}",
        file.references
    );
    assert!(
        file.import_bindings.is_empty(),
        "{:?}",
        file.import_bindings
    );

    let suit = symbol(&file, SymbolKind::Enum, "Suit");
    assert_eq!(
        names_of(&file, SymbolKind::EnumMember),
        ["Hearts", "Spades"]
    );
    assert_eq!(
        symbol(&file, SymbolKind::EnumMember, "Hearts").qualified_name,
        r"App\Cards::Suit::Hearts"
    );
    assert_eq!(
        owned_reference_names(&file, suit, ReferenceKind::Implements),
        ["HasLabel"]
    );
    assert_eq!(
        names_of(&file, SymbolKind::Constant),
        ["MAX", "MIN", "Wild"]
    );
    for constant in ["MAX", "MIN", "Wild"] {
        let constant = symbol(&file, SymbolKind::Constant, constant);
        assert!(
            constant.signature.is_none(),
            "constant values are literals: {constant:?}"
        );
    }
}

/// v1.1.33 declared PHP `property_declaration` members as `field` nodes, and
/// v2 keeps that kind (hooked properties included).
#[test]
fn properties_and_methods_keep_names_signatures_and_visibility() {
    let file = extract("src/Deck.php", DECK_SOURCE);
    assert_eq!(
        names_of(&file, SymbolKind::Field),
        ["count", "legacy", "repo", "untyped"]
    );
    let repo = symbol(&file, SymbolKind::Field, "repo");
    assert_eq!(repo.qualified_name, r"App\Cards::Deck::repo");
    assert_eq!(repo.signature.as_deref(), Some("Repo $repo"));
    assert_eq!(repo.visibility, Some(Visibility::Private));
    assert!(!repo.export.exported);
    assert_eq!(
        owned_reference_names(&file, repo, ReferenceKind::TypeOf),
        ["Repo"]
    );
    let count = symbol(&file, SymbolKind::Field, "count");
    assert_eq!(count.signature.as_deref(), Some("?int $count"));
    assert!(count.execution.static_member);
    assert_eq!(count.visibility, Some(Visibility::Public));
    assert_eq!(
        symbol(&file, SymbolKind::Field, "untyped").visibility,
        Some(Visibility::Protected)
    );
    assert_eq!(
        symbol(&file, SymbolKind::Field, "legacy").visibility,
        Some(Visibility::Public)
    );

    let shuffle = symbol(&file, SymbolKind::Method, "shuffle");
    assert_eq!(
        shuffle.visibility,
        Some(Visibility::Public),
        "PHP defaults to public"
    );
    assert!(shuffle.export.exported);
    let seed = symbol(&file, SymbolKind::Method, "seed");
    assert_eq!(seed.visibility, Some(Visibility::Private));
    assert!(seed.execution.static_member);
    assert!(!seed.export.exported);
}

const CONTROLLER_SOURCE: &str = r"<?php
namespace App\Http\Controllers;

use App\Models\User;
use App\Models\Post as P;
use function App\Support\helper;
use Illuminate\Support\Facades\Cache;

class OrderController extends BaseController
{
    public function index(): void
    {
        $this->authorize();
        self::boot();
        static::make();
        parent::index();
        User::find(1);
        user::where('a');
        P::all();
        Cache::get('k');
        \App\Util::make();
        Sub\Thing::run();
        helper();
        format_date();
        \App\Support\other();
        new User();
        new Order();
        new self();
        new \Vendor\Client();
        $repo->save();
        $this->repo->save();
        getRepo()->save();
        $fn();
        new $cls();
        $class::create();
    }
}
";
const CONTROLLER_KEY: &str = r"App\Http\Controllers::OrderController";

#[test]
fn calls_name_member_static_and_function_targets_with_exact_lookups() {
    let file = extract(
        "app/Http/Controllers/OrderController.php",
        CONTROLLER_SOURCE,
    );
    let index = symbol(&file, SymbolKind::Method, "index");
    assert_eq!(
        index.qualified_name,
        r"App\Http\Controllers::OrderController::index"
    );
    let calls = file
        .references
        .iter()
        .filter(|reference| {
            reference.owner.as_ref() == Some(&index.id) && reference.kind == ReferenceKind::Calls
        })
        .map(|reference| (reference.name.as_str(), reference.resolution_name.clone()))
        .collect::<Vec<_>>();
    let controller = CONTROLLER_KEY;
    let member = |key: &str| Some(lookup("member", key));
    let expected_calls = [
        (
            "$this->authorize",
            Some(lookup(
                "dispatch-member",
                &format!("{controller}::authorize"),
            )),
        ),
        ("self::boot", member(&format!("{controller}::boot"))),
        (
            "static::make",
            Some(lookup("dispatch-member", &format!("{controller}::make"))),
        ),
        (
            "parent::index",
            member(r"App\Http\Controllers::BaseController::index"),
        ),
        ("User::find", member(r"App\Models::User::find")),
        ("user::where", member(r"App\Models::User::where")),
        ("P::all", member(r"App\Models::Post::all")),
        (
            "Cache::get",
            member(r"Illuminate\Support\Facades::Cache::get"),
        ),
        (r"\App\Util::make", member(r"App::Util::make")),
        (
            r"Sub\Thing::run",
            member(r"App\Http\Controllers\Sub::Thing::run"),
        ),
        ("helper", Some(lookup("function", r"App\Support::helper"))),
        (
            "format_date",
            Some(lookup(
                "function-fallback",
                r"App\Http\Controllers::format_date",
            )),
        ),
        (
            r"\App\Support\other",
            Some(lookup("function", r"App\Support::other")),
        ),
        ("$repo->save", None),
        ("$this->repo->save", None),
        (
            "getRepo",
            Some(lookup(
                "function-fallback",
                r"App\Http\Controllers::getRepo",
            )),
        ),
        ("getRepo()->save", None),
        ("$class::create", None),
    ];
    for (name, resolution) in &expected_calls {
        assert!(
            calls.iter().any(
                |(actual, actual_resolution)| actual == name && actual_resolution == resolution
            ),
            "missing call {name} -> {resolution:?}: {calls:#?}"
        );
    }
    assert_eq!(
        calls.len(),
        expected_calls.len(),
        "a variable callee ($fn()) is not a call target: {calls:#?}"
    );
}

#[test]
fn constructions_and_inheritance_name_classes_with_exact_lookups() {
    let file = extract(
        "app/Http/Controllers/OrderController.php",
        CONTROLLER_SOURCE,
    );
    let instantiations = file
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Instantiates)
        .map(|reference| (reference.name.as_str(), reference.resolution_name.clone()))
        .collect::<Vec<_>>();
    let class = |key: &str| Some(lookup("class", key));
    assert_eq!(
        instantiations,
        [
            ("User", class(r"App\Models::User")),
            ("Order", class(r"App\Http\Controllers::Order")),
            ("self", class(CONTROLLER_KEY)),
            (r"\Vendor\Client", class(r"Vendor::Client")),
        ],
        "new $cls() is dynamic and must stay unextracted"
    );

    let controller_class = symbol(&file, SymbolKind::Class, "OrderController");
    let extends = reference(&file, ReferenceKind::Extends, "BaseController");
    assert_eq!(extends.owner.as_ref(), Some(&controller_class.id));
    assert_eq!(
        extends.resolution_name,
        class(r"App\Http\Controllers::BaseController")
    );
}

#[test]
fn callable_types_signatures_and_builtins_follow_v1_type_reference_rules() {
    let file = extract(
        "src/ApiClient.php",
        r"<?php
namespace Acme;

use Acme\Models\User;

class ApiClient {
    public static function for(string $credential): ?self { return new self(); }
    public function load(Config $cfg, ?\Acme\Opts $o, int ...$rest): User|Guest { return $cfg; }
    public function secret(string $token = 'sk_live_never_copied'): void {}
}

function run(ApiClient $client): void {}
",
    );
    let factory = symbol(&file, SymbolKind::Method, "for");
    assert_eq!(
        factory.signature.as_deref(),
        Some("(string $credential): ?self")
    );
    assert!(factory.execution.static_member);
    assert!(
        owned_reference_names(&file, factory, ReferenceKind::Returns).is_empty(),
        "self is a builtin type position, not a class reference"
    );

    let load = symbol(&file, SymbolKind::Method, "load");
    assert_eq!(
        owned_reference_names(&file, load, ReferenceKind::TypeOf),
        ["Config", r"\Acme\Opts"]
    );
    assert_eq!(
        owned_reference_names(&file, load, ReferenceKind::Returns),
        ["Guest", "User"]
    );
    let returns_user = file
        .references
        .iter()
        .find(|reference| {
            reference.owner.as_ref() == Some(&load.id)
                && reference.kind == ReferenceKind::Returns
                && reference.name == "User"
        })
        .unwrap_or_else(|| panic!("missing User return"));
    assert_eq!(
        returns_user.resolution_name,
        Some(lookup("class", r"Acme\Models::User")),
        "an imported name expands through its use alias"
    );
    let opts = file
        .references
        .iter()
        .find(|reference| reference.name == r"\Acme\Opts")
        .unwrap_or_else(|| panic!("missing Opts type"));
    assert_eq!(opts.resolution_name, Some(lookup("class", r"Acme::Opts")));

    let secret = symbol(&file, SymbolKind::Method, "secret");
    assert!(
        secret.signature.is_none(),
        "literal defaults must never reach a signature: {secret:?}"
    );
    assert!(
        !file.symbols.iter().any(|symbol| symbol
            .signature
            .as_deref()
            .is_some_and(|signature| signature.contains("sk_live"))),
        "no symbol may retain the literal default"
    );

    let run = symbol(&file, SymbolKind::Function, "run");
    assert_eq!(run.qualified_name, r"Acme::run");
    assert!(run.export.exported);
    assert_eq!(
        owned_reference_names(&file, run, ReferenceKind::TypeOf),
        ["ApiClient"]
    );
}

#[test]
fn namespaces_scope_declarations_and_imports_per_block() {
    let file = extract(
        "src/multi.php",
        r"<?php
namespace First;
use Shared\Thing;
class A { function go() { new Thing(); } }

namespace Second;
class B { function go() { new Thing(); } }

namespace Third {
    function helper() {}
}

namespace {
    function global_helper() {}
}
",
    );
    assert_eq!(
        symbol(&file, SymbolKind::Class, "A").qualified_name,
        "First::A"
    );
    assert_eq!(
        symbol(&file, SymbolKind::Class, "B").qualified_name,
        "Second::B"
    );
    assert_eq!(
        symbol(&file, SymbolKind::Function, "helper").qualified_name,
        "Third::helper"
    );
    assert_eq!(
        symbol(&file, SymbolKind::Function, "global_helper").qualified_name,
        "global_helper"
    );
    let second = symbol(&file, SymbolKind::Namespace, "Second");
    assert!(
        !file
            .containments
            .iter()
            .any(|containment| containment.child == second.id),
        "a later unbraced namespace is a sibling, not a child, of the previous one"
    );
    let resolutions = file
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Instantiates)
        .map(|reference| reference.resolution_name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        resolutions,
        [
            Some(lookup("class", r"Shared::Thing")),
            Some(lookup("class", "Second::Thing"))
        ],
        "a use import applies only to its own namespace block"
    );
}

fn route<'file>(file: &'file ExtractedFile, name: &str) -> &'file ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Route && symbol.name == name)
        .unwrap_or_else(|| panic!("missing route {name}: {:#?}", file.symbols))
}

fn route_target<'file>(
    file: &'file ExtractedFile,
    route: &ExtractedSymbol,
) -> &'file ExtractedReference {
    let targets = file
        .references
        .iter()
        .filter(|reference| {
            reference.owner.as_ref() == Some(&route.id) && reference.kind == ReferenceKind::Calls
        })
        .collect::<Vec<_>>();
    let [target] = targets.as_slice() else {
        panic!(
            "route {} must call exactly one handler: {targets:#?}",
            route.name
        );
    };
    target
}

#[test]
fn symfony_attribute_routes_join_class_prefixes_and_call_their_actions() {
    let v1 = extract(
        "src/Controller/OrderController.php",
        "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\nclass OrderController {\n  #[Route('/orders/{id}', name: 'orders_show', methods: ['GET'])]\n  public function show() {}\n}\n",
    );
    let orders = route(&v1, "orders_show");
    assert!(
        orders.body_search_text.contains("GET /orders/{id}"),
        "{orders:?}"
    );
    let target = route_target(&v1, orders);
    assert_eq!(target.name, "show");
    assert_eq!(
        target.resolution_name.as_deref(),
        Some("OrderController::show")
    );

    let file = extract(
        "src/Controller/BlogController.php",
        r"<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route;

#[Route('/blog', name: 'blog_')]
class BlogController
{
    #[Route('/list', name: 'list', methods: ['GET'])]
    public function list(): void {}

    #[Route(path: '/{slug}', name: 'show', methods: ['GET', 'HEAD'])]
    #[Route('/legacy/{slug}')]
    public function show(): void {}

    // #[Route('/commented', name: 'commented')]
    #[Route(['en' => '/about', 'nl' => '/over-ons'], name: 'about')]
    public function about(): void {}

    public function helper(): void {}
}
",
    );
    let list = route(&file, "blog_list");
    assert!(list.body_search_text.contains("GET /blog/list"), "{list:?}");
    let target = route_target(&file, list);
    assert_eq!(target.name, "list");
    assert_eq!(
        target.resolution_name.as_deref(),
        Some(r"App\Controller::BlogController::list")
    );
    let show = route(&file, "blog_show");
    assert!(
        show.body_search_text.contains("GET|HEAD /blog/{slug}"),
        "{show:?}"
    );
    assert_eq!(route_target(&file, show).name, "show");
    let legacy = route(&file, "/blog/legacy/{slug}");
    assert!(
        legacy.body_search_text.contains("ANY /blog/legacy/{slug}"),
        "{legacy:?}"
    );
    assert_eq!(route_target(&file, legacy).name, "show");
    assert_eq!(
        names_of(&file, SymbolKind::Route),
        ["/blog/legacy/{slug}", "blog_list", "blog_show"],
        "class prefixes, commented attributes, and localized path maps are not routes"
    );

    let unrelated = extract(
        "src/Controller/Other.php",
        "<?php\nuse Spatie\\RouteAttributes\\Attributes\\Route;\nclass Other {\n  #[Route('get', '/other')]\n  public function other() {}\n}\n",
    );
    assert!(
        names_of(&unrelated, SymbolKind::Route).is_empty(),
        "a Route attribute without Symfony routing is not a Symfony route: {:?}",
        unrelated.symbols
    );
}

#[test]
fn symfony_attribute_routes_follow_the_attribute_loader_composition_rules() {
    let merged = extract(
        "src/Controller/ApiController.php",
        r"<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route as R;

#[R('/api', name: 'api_', methods: ['GET'])]
final class ApiController
{
    #[R('/items', name: 'items', methods: ['POST', 'get'])]
    public function items(): void {}

    #[\Symfony\Component\Routing\Attribute\Route(name: 'root')]
    public function root(): void {}
}
",
    );
    let items = route(&merged, "api_items");
    assert!(
        items.body_search_text.contains("GET|POST /api/items"),
        "class methods merge before route methods: {items:?}"
    );
    let root = route(&merged, "api_root");
    assert!(
        root.body_search_text.contains("GET /api"),
        "an omitted path inherits the class prefix: {root:?}"
    );

    let invokable = extract(
        "src/Controller/HealthController.php",
        r"<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route;

#[Route('/health', name: 'health', methods: ['GET'])]
final class HealthController
{
    public function __invoke(): void {}
}
",
    );
    let health = route(&invokable, "health");
    let target = route_target(&invokable, health);
    assert_eq!(target.name, "__invoke");
    assert_eq!(
        target.resolution_name.as_deref(),
        Some(r"App\Controller::HealthController::__invoke")
    );

    let computed = extract(
        "src/Controller/ComputedController.php",
        r"<?php
use Symfony\Component\Routing\Attribute\Route;

#[Route(self::PREFIX)]
final class ComputedController
{
    const PREFIX = '/computed';

    #[Route('/list', name: 'list')]
    public function list(): void {}
}
",
    );
    assert!(
        names_of(&computed, SymbolKind::Route).is_empty(),
        "a computed class prefix is unknown, not empty: {:?}",
        computed.symbols
    );

    let shadowed = extract(
        "src/Controller/ShadowController.php",
        r"<?php
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;
use App\Routing\Route;

final class ShadowController
{
    #[Route('/shadow', name: 'shadow')]
    public function shadow(): void {}
}
",
    );
    assert!(
        names_of(&shadowed, SymbolKind::Route).is_empty(),
        "a Route attribute that resolves to another class is not a Symfony route: {:?}",
        shadowed.symbols
    );
}

#[test]
fn signatures_drop_comments_and_attributes_and_attributes_decorate_declarations() {
    let file = extract(
        "src/Vault.php",
        r"<?php
namespace App;

use App\Models\{
    // sk_live_import_canary
    User
};

#[Entity(table: Vault::TABLE)]
final class Vault
{
    #[Pure]
    public function open(/* sk_live_review_canary */ int $slot, #[\SensitiveParameter] string $pin): void {}
}
",
    );
    let open = symbol(&file, SymbolKind::Method, "open");
    assert_eq!(
        open.signature.as_deref(),
        Some("(int $slot, string $pin): void"),
        "comments and attributes are not part of a signature"
    );
    assert!(
        !file.symbols.iter().any(|symbol| symbol
            .signature
            .as_deref()
            .is_some_and(|signature| signature.contains("canary"))),
        "comment text never reaches a signature"
    );
    let import = symbol(&file, SymbolKind::Import, r"App\Models\User");
    assert_eq!(
        import.signature.as_deref(),
        Some(r"use App\Models\{ User };"),
        "an import signature keeps the statement without comments"
    );
    let vault = symbol(&file, SymbolKind::Class, "Vault");
    let entity = reference(&file, ReferenceKind::Decorates, "Entity");
    assert_eq!(entity.owner.as_ref(), Some(&vault.id));
    assert_eq!(entity.resolution_name, Some(lookup("class", "App::Entity")));
    let pure = reference(&file, ReferenceKind::Decorates, "Pure");
    assert_eq!(pure.owner.as_ref(), Some(&open.id));
}

#[test]
fn hooks_nested_declarations_and_anonymous_class_arguments_keep_their_scopes() {
    let file = extract(
        "src/Scopes.php",
        r"<?php
namespace N;

class Outer
{
    public string $label { get { return format_label($this->raw); } }

    public function make(): object
    {
        return new class($this->helper(), new self()) {
            public function inside(): void { $this->own(); }
        };
    }

    private function helper(): int { return 1; }
}

function outer(): void
{
    function inner(): void {}
    inner();
    self::nowhere();
}
",
    );
    let label = symbol(&file, SymbolKind::Field, "label");
    assert_eq!(
        owned_reference_names(&file, label, ReferenceKind::Calls),
        ["format_label"],
        "property hook bodies are visited and owned by their property"
    );
    let make = symbol(&file, SymbolKind::Method, "make");
    let calls = file
        .references
        .iter()
        .filter(|reference| reference.owner.as_ref() == Some(&make.id))
        .map(|reference| (reference.name.as_str(), reference.resolution_name.clone()))
        .collect::<Vec<_>>();
    assert!(
        calls.contains(&(
            "$this->helper",
            Some(lookup("dispatch-member", "N::Outer::helper"))
        )),
        "anonymous-class constructor arguments run in the enclosing class: {calls:#?}"
    );
    assert!(
        calls.contains(&("self", Some(lookup("class", "N::Outer")))),
        "{calls:#?}"
    );
    let inside = symbol(&file, SymbolKind::Method, "inside");
    let own = reference(&file, ReferenceKind::Calls, "$this->own");
    assert_eq!(own.owner.as_ref(), Some(&inside.id));
    assert_eq!(
        own.resolution_name, None,
        "an anonymous class has no name to resolve $this against"
    );

    let inner = symbol(&file, SymbolKind::Function, "inner");
    assert_eq!(
        inner.qualified_name, "N::inner",
        "a function declared in a function body is a namespace-level function"
    );
    let call = reference(&file, ReferenceKind::Calls, "inner");
    assert_eq!(
        call.resolution_name,
        Some(lookup("function-fallback", "N::inner"))
    );
    let nowhere = reference(&file, ReferenceKind::Calls, "self::nowhere");
    assert_eq!(
        nowhere.resolution_name,
        Some(lookup("abstain", "self")),
        "self outside a class abstains instead of falling back to short names"
    );
}

#[test]
fn symfony_attribute_routes_respect_import_scope_conditional_classes_and_handler_spans() {
    let scoped = extract(
        "src/Controller/Scoped.php",
        r"<?php
namespace A {
    use Symfony\Component\Routing\Attribute\Route as R;
    class First { #[R('/first', name: 'first')] public function go() {} }
}
namespace B {
    use App\Other\Route as R;
    use function Symfony\Component\Routing\Attribute\Route;
    class Second {
        #[R('/second', name: 'second')] public function go() {}
        #[Route('/third', name: 'third')] public function stop() {}
    }
}
",
    );
    assert_eq!(
        names_of(&scoped, SymbolKind::Route),
        ["first"],
        "an alias imports only into its own namespace block and symbol space"
    );

    let conditional = extract(
        "src/Controller/Conditional.php",
        r"<?php
use Symfony\Component\Routing\Attribute\Route;
if (PHP_VERSION_ID >= 80000) {
    #[Route('/a')]
    class Conditional { #[Route('/x', name: 'x')] public function x() {} }
} else {
    #[Route('/b')]
    class Conditional { #[Route('/y', name: 'y')] public function y() {} }
}
",
    );
    let x = route(&conditional, "x");
    let y = route(&conditional, "y");
    assert!(x.body_search_text.contains("ANY /a/x"), "{x:?}");
    assert!(y.body_search_text.contains("ANY /b/y"), "{y:?}");

    let source = "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\nclass Fun {\n  #[Route('/fun', name: 'fun')]\n  public function fun() {}\n}\n";
    let fun = extract("src/Controller/Fun.php", source);
    let target = route_target(&fun, route(&fun, "fun"));
    let start = usize::try_from(target.span.start_byte())
        .unwrap_or_else(|error| panic!("span start does not fit usize: {error}"));
    let end = usize::try_from(target.span.end_byte())
        .unwrap_or_else(|error| panic!("span end does not fit usize: {error}"));
    assert_eq!(&source[start..end], "fun");
    assert_eq!(
        &source[start.saturating_sub("function ".len())..start],
        "function ",
        "the handler span is the method name, not a slice of the keyword"
    );
}

#[test]
fn traits_hooks_and_unsupported_namespaces_abstain_where_the_target_is_unknown() {
    let file = extract(
        "src/Traits.php",
        r"<?php
namespace N;

trait Greets
{
    public function make(): static { return new self(); }
    public function greet(): void { self::hello(); $this->hello(); }
    public function hello(): void {}
}

class Holder
{
    public string $p {
        get {
            function hook_helper(): string { return ''; }
            return hook_helper();
        }
    }
}
",
    );
    let make = symbol(&file, SymbolKind::Method, "make");
    let construction = file
        .references
        .iter()
        .find(|reference| {
            reference.owner.as_ref() == Some(&make.id)
                && reference.kind == ReferenceKind::Instantiates
        })
        .unwrap_or_else(|| panic!("missing new self in trait"));
    assert_eq!(
        construction.resolution_name,
        Some(lookup("abstain", "self")),
        "a trait never constructs itself; the using class is unknown here"
    );
    let greet = symbol(&file, SymbolKind::Method, "greet");
    for name in ["self::hello", "$this->hello"] {
        let call = file
            .references
            .iter()
            .find(|reference| reference.owner.as_ref() == Some(&greet.id) && reference.name == name)
            .unwrap_or_else(|| panic!("missing trait call {name}"));
        assert_eq!(
            call.resolution_name,
            Some(lookup("dispatch-member", "N::Greets::hello")),
            "{name} in a trait is late-bound to the using class"
        );
    }
    assert_eq!(
        symbol(&file, SymbolKind::Function, "hook_helper").qualified_name,
        "N::hook_helper",
        "a function declared in a property hook is a namespace-level function"
    );

    let namespace = vec!["Segment"; 80].join("\\");
    let oversized = extract(
        "src/Oversized.php",
        &format!("<?php\nnamespace {namespace};\nfunction run() {{ new Target(); helper(); }}\n"),
    );
    let target = reference(&oversized, ReferenceKind::Instantiates, "Target");
    assert_eq!(
        target.resolution_name,
        Some(lookup("abstain", "Target")),
        "an unrepresentable namespace is not the global namespace"
    );
    assert_eq!(
        reference(&oversized, ReferenceKind::Calls, "helper").resolution_name,
        Some(lookup("abstain", "helper"))
    );
}

#[test]
fn symfony_route_paths_normalize_the_joined_leading_slashes() {
    for (prefix, path, expected) in [
        ("", "health", "/health"),
        ("", "///health", "/health"),
        ("", "  //health  ", "/health"),
        ("", "\t //health \u{b}", "/health"),
        ("", "///", "/"),
        (" //api", "/health ", "/api/health"),
        ("", "/health//detail/", "/health//detail/"),
        ("", "\u{a0}health", "/\u{a0}health"),
    ] {
        let source = format!(
            "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n#[Route('{prefix}')]\nclass Health {{\n  #[Route('{path}', name: 'health')]\n  public function health() {{}}\n}}\n"
        );
        let file = extract("src/Controller/Health.php", &source);
        let health = route(&file, "health");
        assert_eq!(
            health.body_search_text,
            format!("symfony route health ANY {expected}")
        );
        assert!(
            health
                .qualified_name
                .contains(&format!("symfony-route::ANY::{expected}::health")),
            "{health:?}"
        );
        assert_eq!(route_target(&file, health).name, "health");
    }
    let unnamed = extract(
        "src/Controller/Unnamed.php",
        "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n#[Route('///health')]\nclass Health { public function __invoke() {} }\n",
    );
    let health = route(&unnamed, "/health");
    assert_eq!(health.body_search_text, "symfony route /health ANY /health");
    assert_eq!(route_target(&unnamed, health).name, "__invoke");
}

#[test]
fn symfony_normalized_route_paths_reject_sensitive_content() {
    const CANARY: &str = "sk_live_CARTOGRAPH_REVIEW_CANARY";
    for (prefix, path) in [
        ("", CANARY.to_owned()),
        ("", format!("  ///{CANARY}  ")),
        (CANARY, "/secret".to_owned()),
    ] {
        let source = format!(
            "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n#[Route('{prefix}')]\nclass Sensitive {{\n  #[Route('{path}', name: 'secret')]\n  public function secret() {{}}\n}}\nclass Health {{\n  #[Route('health', name: 'health')]\n  public function health() {{}}\n}}\n"
        );
        let file = extract("src/Controller/Sensitive.php", &source);
        let serialized = serde_json::to_string(&file)
            .unwrap_or_else(|error| panic!("fact serialization failed: {error}"));
        assert!(
            !serialized.contains(CANARY),
            "sensitive route content leaked"
        );
        assert_eq!(names_of(&file, SymbolKind::Route), ["health"]);
        let health = route(&file, "health");
        assert_eq!(health.body_search_text, "symfony route health ANY /health");
        assert!(
            health
                .qualified_name
                .contains("symfony-route::ANY::/health::health")
        );
    }
}

#[test]
fn symfony_route_segments_reject_sensitive_content() {
    const CANARY: &str = "sk_live_CARTOGRAPH_REVIEW_CANARY";
    for (prefix, path) in [
        ("/api", format!("/{CANARY}")),
        ("/api", format!("/nested/{CANARY}")),
        ("/api", CANARY.to_owned()),
        (
            "/api/sk_live_CARTOGRAPH_REVIEW_CANARY",
            "/secret".to_owned(),
        ),
        ("/api/sk_", "live_CARTOGRAPH_REVIEW_CANARY".to_owned()),
    ] {
        let source = format!(
            "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n#[Route('{prefix}')]\nclass Sensitive {{\n  #[Route('{path}', name: 'secret')]\n  public function secret() {{}}\n}}\n#[Route('/api')]\nclass Health {{\n  #[Route('/health', name: 'health')]\n  public function health() {{}}\n}}\n"
        );
        let file = extract("src/Controller/Segments.php", &source);
        let serialized = serde_json::to_string(&file)
            .unwrap_or_else(|error| panic!("fact serialization failed: {error}"));
        assert!(
            !serialized.contains(CANARY),
            "sensitive route segment leaked"
        );
        assert_eq!(names_of(&file, SymbolKind::Route), ["health"]);
        let health = route(&file, "health");
        assert_eq!(
            health.body_search_text,
            "symfony route health ANY /api/health"
        );
        assert!(
            health
                .qualified_name
                .contains("symfony-route::ANY::/api/health::health")
        );
    }
}

#[test]
fn symfony_route_paths_abstain_if_validation_trims_significant_bytes() {
    for suffix in ['\u{a0}', '\u{c}'] {
        let source = format!(
            "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\nclass Health {{\n  #[Route('health{suffix}', name: 'health')]\n  public function health() {{}}\n}}\n"
        );
        let file = extract("src/Controller/Whitespace.php", &source);
        assert_eq!(names_of(&file, SymbolKind::Route), [] as [&str; 0]);
    }
}

#[test]
fn symfony_route_names_abstain_if_validation_trims_significant_bytes() {
    for (prefix, name) in [
        ("", " health"),
        ("", "health "),
        ("", "health\u{a0}"),
        ("api_", "health\u{a0}"),
        ("api_\u{a0}", ""),
    ] {
        let source = format!(
            "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n#[Route('/api', name: '{prefix}')]\nclass Health {{\n  #[Route('/health', name: '{name}')]\n  public function health() {{}}\n  #[Route('/neighbor', name: 'neighbor')]\n  public function neighbor() {{}}\n}}\n"
        );
        let file = extract("src/Controller/Names.php", &source);
        assert_eq!(
            names_of(&file, SymbolKind::Route),
            [format!("{prefix}neighbor")]
        );
    }
}

#[test]
fn symfony_routes_handle_unicode_aliases_root_paths_and_invokable_handler_spans() {
    let unicode = extract(
        "src/Controller/Unicode.php",
        "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route as R\u{e9};\n#[R\u{e9}('/api')]\nclass Unicode {\n  #[R\u{e9}('/x', name: 'x')]\n  public function x() {}\n}\n",
    );
    let x = route(&unicode, "x");
    assert!(
        x.body_search_text.contains("ANY /api/x"),
        "a non-ASCII alias keeps its arguments: {x:?}"
    );

    let root = extract(
        "src/Controller/Root.php",
        "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\nclass Root {\n  #[Route(path: null, name: 'root')]\n  public function home() {}\n}\n",
    );
    let home = route(&root, "root");
    assert!(
        home.body_search_text.contains("ANY /"),
        "an empty route path is the root path: {home:?}"
    );

    let source = "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n#[Route('/invoke', name: 'invoke')]\nclass Invoke {\n  #[Other('function aaaaaaa\u{e9}')]\n  public function __invoke() {}\n}\n";
    let invokable = extract("src/Controller/Invoke.php", source);
    let target = route_target(&invokable, route(&invokable, "invoke"));
    let start = usize::try_from(target.span.start_byte())
        .unwrap_or_else(|error| panic!("span start does not fit usize: {error}"));
    let end = usize::try_from(target.span.end_byte())
        .unwrap_or_else(|error| panic!("span end does not fit usize: {error}"));
    assert_eq!(
        &source[start..end],
        "__invoke",
        "the handler span skips attribute text that mentions `function`"
    );
}

/// Receivers that are and are not statically named factory calls.
const FACTORY_CHAIN_SOURCE: &str = r"<?php
namespace App\Shop;

use App\Api\Client;

class Checkout extends \App\Base
{
    public function pay(): void
    {
        Client::connect('k')?->send();
        \Other\Gateway::open()->charge();
        static::make()->save();
        $this->client()->send();
        Checkout::create()->send();
        parent::make()->save();
        Client::connect()->retry()->send();
        $client->connect()->send();
        $class::make()->save();
        make_client()->send();
    }

    public function closures(): void
    {
        Client::connect(...)->call($this);
        $this->client(...)->bindTo($this);
        Client::connect(/* a note */ ...)->bindTo($this);
    }

    public function install(): void
    {
        function nested_checkout(): void
        {
            Checkout::create()->send();
            self::create();
        }
    }
}
function outside(): void
{
    self::make()->save();
}
";

#[test]
fn factory_chains_record_the_exact_factory_and_returned_member() {
    let file = extract("src/Shop/Checkout.php", FACTORY_CHAIN_SOURCE);
    let pay = symbol(&file, SymbolKind::Method, "pay");
    let owned = |owner: &ExtractedSymbol, name: &str| {
        file.references
            .iter()
            .find(|reference| {
                reference.owner.as_ref() == Some(&owner.id)
                    && reference.kind == ReferenceKind::Calls
                    && reference.name == name
            })
            .unwrap_or_else(|| panic!("missing call {name}: {:#?}", file.references))
            .resolution_name
            .clone()
    };
    let returned = |key: &str| Some(lookup("returned-member", key));
    let own = |key: &str| Some(lookup("own-returned-member", key));
    assert_eq!(
        owned(pay, "Client::connect()?->send"),
        returned(r"App\Api::Client::connect::send")
    );
    assert_eq!(
        owned(pay, r"\Other\Gateway::open()->charge"),
        returned(r"Other::Gateway::open::charge")
    );
    assert_eq!(
        owned(pay, "static::make()->save"),
        own(r"App\Shop::Checkout::make::save"),
        "a call inside the factory's own class may reach a non-public factory"
    );
    assert_eq!(
        owned(pay, "$this->client()->send"),
        own(r"App\Shop::Checkout::client::send")
    );
    assert_eq!(
        owned(pay, "Checkout::create()->send"),
        own(r"App\Shop::Checkout::create::send")
    );
    assert_eq!(
        owned(pay, "parent::make()->save"),
        returned(r"App::Base::make::save"),
        "a parent class is outside the caller's own class"
    );
    assert_eq!(
        owned(pay, "Client::connect()->retry"),
        returned(r"App\Api::Client::connect::retry")
    );
    for unknown in [
        "Client::connect()->retry()->send",
        "$client->connect()->send",
        "$class::make()->save",
        "make_client()->send",
    ] {
        assert_eq!(
            owned(pay, unknown),
            None,
            "only a statically known factory one call deep names a receiver: {unknown}"
        );
    }
    let outside = symbol(&file, SymbolKind::Function, "outside");
    assert_eq!(
        owned(outside, "self::make()->save"),
        None,
        "self outside a class has no factory to follow"
    );
    let closures = symbol(&file, SymbolKind::Method, "closures");
    for callable in [
        "Client::connect()->call",
        "$this->client()->bindTo",
        "Client::connect()->bindTo",
    ] {
        assert_eq!(
            owned(closures, callable),
            None,
            "a first-class callable receiver is a Closure, not the factory's result: {callable}"
        );
    }
    let nested = symbol(&file, SymbolKind::Function, "nested_checkout");
    assert_eq!(
        owned(nested, "Checkout::create()->send"),
        returned(r"App\Shop::Checkout::create::send"),
        "a named function has no class scope even inside a method, so it is not the own class"
    );
    assert_eq!(
        owned(nested, "self::create"),
        Some(lookup("abstain", "self")),
        "self names no class inside a named function"
    );
}

#[test]
fn first_class_callable_detection_skips_a_bounded_number_of_comments() {
    let commented = |comments: usize, argument: &str| {
        let file = extract(
            "src/Commented.php",
            &format!(
                "<?php\nclass C {{ public static function make(): self {{ return new self(); }} }}\nfunction f(): void {{ C::make({} {argument})->bindTo(null); }}\n",
                "/* c */ ".repeat(comments)
            ),
        );
        file.references
            .iter()
            .find(|reference| reference.name == "C::make()->bindTo")
            .unwrap_or_else(|| panic!("missing commented call: {:#?}", file.references))
            .resolution_name
            .clone()
    };
    assert_eq!(
        commented(2, "$x"),
        Some(lookup("returned-member", "C::make::bindTo"))
    );
    assert_eq!(commented(64, "..."), None);
    assert_eq!(
        commented(64, "$x"),
        None,
        "past the leading-comment bound the call may be first-class and abstains"
    );
}

#[test]
fn adapted_trait_uses_are_marked_because_they_can_rename_methods() {
    let file = extract(
        "src/Tagged.php",
        r"<?php
namespace App;

class Tagged
{
    use Plain;
    use Queries, Other { search as where; Other::find insteadof Queries; }
}
",
    );
    let tagged = symbol(&file, SymbolKind::Class, "Tagged");
    let implemented = |name: &str| {
        file.references
            .iter()
            .find(|reference| {
                reference.owner.as_ref() == Some(&tagged.id)
                    && reference.kind == ReferenceKind::Implements
                    && reference.name == name
            })
            .unwrap_or_else(|| panic!("missing trait use {name}: {:#?}", file.references))
            .resolution_name
            .clone()
    };
    assert_eq!(implemented("Plain"), Some(lookup("class", "App::Plain")));
    assert_eq!(
        implemented("Queries"),
        Some(lookup("adapted-class", "App::Queries"))
    );
    assert_eq!(
        implemented("Other"),
        Some(lookup("adapted-class", "App::Other"))
    );
}
