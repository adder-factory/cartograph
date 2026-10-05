//! Python, Go, and Rust declarations and usages that the v1 extractor
//! published: module-level bindings, struct fields, embedding and supertrait
//! inheritance, decorators, literal construction, cgo calls, annotated type
//! consumers, and constant reads inside bodies.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{
    ExtractedFile, ExtractedReference, ExtractedSymbol, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT_BYTES: usize = 256 * 1024;
/// The empty name list, for asserting that nothing was recorded.
const NONE: [&str; 0] = [];

#[test]
fn python_module_level_assignments_declare_variables_once() {
    let file = extract(
        "app/settings.py",
        r"
X = 1
config = load()
router = APIRouter()
registry: Registry = Registry()
_private = config
a, b = pair()
DEBUG = flag
DEBUG = other_flag

if enabled:
    Z = build()

class K:
    LIMIT = 5

def run():
    local = make()
    return local
",
    );

    assert_eq!(
        names_of_kind(&file, SymbolKind::Variable),
        [
            "DEBUG", "X", "Z", "_private", "config", "registry", "router"
        ]
    );
    assert!(
        symbol(&file, "X").signature.is_none(),
        "literal initializer"
    );
    assert_eq!(
        symbol(&file, "config").signature.as_deref(),
        Some("= load()")
    );
    assert!(symbol(&file, "router").export.exported);
    assert!(!symbol(&file, "_private").export.exported);
    for absent in ["a", "b", "K::LIMIT", "LIMIT", "local", "run::local"] {
        assert!(
            file.symbols
                .iter()
                .all(|entry| entry.qualified_name != absent),
            "{absent} must not be a symbol"
        );
    }
    assert!(has_reference(
        &file,
        "router",
        "APIRouter",
        ReferenceKind::Calls
    ));
    assert!(has_reference(
        &file,
        "registry",
        "Registry",
        ReferenceKind::TypeOf
    ));
    assert!(has_reference(
        &file,
        "registry",
        "Registry",
        ReferenceKind::Calls
    ));
    assert!(has_reference(&file, "run", "make", ReferenceKind::Calls));
}

#[test]
fn python_staticmethod_marks_static_members_only() {
    let file = extract(
        "app/k.py",
        r"
class K:
    @staticmethod
    def build():
        pass

    @classmethod
    def create(cls):
        pass

    @trace
    def plain(self):
        pass
",
    );

    assert!(symbol(&file, "K::build").execution.static_member);
    assert!(!symbol(&file, "K::create").execution.static_member);
    assert!(!symbol(&file, "K::plain").execution.static_member);
}

#[test]
fn python_protocol_and_abc_bases_are_implements() {
    let file = extract(
        "app/widget.py",
        r"
from typing import Protocol
import abc
import typing

class Bar:
    pass

class Renderable(Protocol):
    def render(self) -> str: ...

class Widget(Bar, Renderable):
    pass

class Shape(abc.ABC):
    pass

class Drawable(typing.Protocol, metaclass=ABCMeta):
    pass

class Meta(ABCMeta):
    pass

class MyProtocol:
    pass

class ABCDishwasher:
    pass

class Foo(MyProtocol, ABCDishwasher):
    pass
",
    );

    assert!(has_reference(
        &file,
        "Renderable",
        "Protocol",
        ReferenceKind::Implements
    ));
    assert!(has_reference(
        &file,
        "Shape",
        "abc.ABC",
        ReferenceKind::Implements
    ));
    assert!(has_reference(
        &file,
        "Drawable",
        "typing.Protocol",
        ReferenceKind::Implements
    ));
    assert!(has_reference(
        &file,
        "Meta",
        "ABCMeta",
        ReferenceKind::Implements
    ));
    assert!(has_reference(
        &file,
        "Widget",
        "Bar",
        ReferenceKind::Extends
    ));
    assert!(has_reference(
        &file,
        "Widget",
        "Renderable",
        ReferenceKind::Extends
    ));
    assert!(has_reference(
        &file,
        "Foo",
        "MyProtocol",
        ReferenceKind::Extends
    ));
    assert!(has_reference(
        &file,
        "Foo",
        "ABCDishwasher",
        ReferenceKind::Extends
    ));
    for (owner, base) in [
        ("Renderable", "Protocol"),
        ("Shape", "abc.ABC"),
        ("Foo", "MyProtocol"),
        ("Foo", "ABCDishwasher"),
    ] {
        let kinds = reference_kinds(&file, owner, base);
        assert_eq!(kinds.len(), 1, "{owner} -> {base}: {kinds:?}");
    }
    assert_eq!(
        reference_kinds(&file, "Drawable", "ABCMeta"),
        [] as [ReferenceKind; 0],
        "a metaclass keyword is not a base"
    );
}

#[test]
fn python_decorators_decorate_their_own_definition() {
    let file = extract(
        "app/views.py",
        r"
@dataclass
class Model:
    pass

class Service:
    @app.route('/x')
    def handler(self):
        pass

    @functools.cache
    def cached(self):
        pass

@first
def alpha():
    pass

@second
def beta():
    pass
",
    );

    assert!(has_reference(
        &file,
        "Model",
        "dataclass",
        ReferenceKind::Decorates
    ));
    let route = reference(&file, "Service::handler", "route", ReferenceKind::Decorates);
    assert_eq!(route.resolution_name.as_deref(), Some("app.route"));
    let cache = reference(&file, "Service::cached", "cache", ReferenceKind::Decorates);
    assert_eq!(cache.resolution_name.as_deref(), Some("functools.cache"));
    assert_eq!(decorators_of(&file, "alpha"), ["first"]);
    assert_eq!(decorators_of(&file, "beta"), ["second"]);
    assert_eq!(decorators_of(&file, "Service"), NONE);
    assert!(has_reference(
        &file,
        "Service",
        "app.route",
        ReferenceKind::Calls
    ));
}

#[test]
fn python_annotated_attributes_and_locals_are_type_consumers() {
    let file = extract(
        "app/box.py",
        r"
class Box:
    field: Foo
    cache: dict

def process(a):
    local: Local = a
    return local
",
    );

    assert!(has_reference(&file, "Box", "Foo", ReferenceKind::TypeOf));
    assert!(has_reference(
        &file,
        "process",
        "Local",
        ReferenceKind::TypeOf
    ));
    assert_eq!(names_of_kind(&file, SymbolKind::Variable), NONE);
}

#[test]
fn go_var_and_const_blocks_declare_package_bindings() {
    let file = extract(
        "api/blocks.go",
        r"
package api

var (
    Alpha = 1
    bravo = helper()
    Charlie = 3
)

const (
    DELTA = 10
    echo  = 20
)

const (
    First Kind = iota
    Second
)

var Single = 99
var left, right Cache
var _ Runner = (*Worker)(nil)

func process(b Bar) {
    var local Local = b
    const limit Limit = 3
    _ = local
}
",
    );

    assert_eq!(
        names_of_kind(&file, SymbolKind::Variable),
        ["Alpha", "Charlie", "Single", "bravo", "left", "right"]
    );
    assert_eq!(
        names_of_kind(&file, SymbolKind::Constant),
        ["DELTA", "First", "Second", "echo"]
    );
    assert!(symbol(&file, "Alpha").export.exported);
    assert!(!symbol(&file, "bravo").export.exported);
    assert_eq!(
        symbol(&file, "bravo").signature.as_deref(),
        Some("= helper()")
    );
    assert!(has_reference(
        &file,
        "bravo",
        "helper",
        ReferenceKind::Calls
    ));
    assert!(has_reference(&file, "First", "Kind", ReferenceKind::TypeOf));
    assert!(has_reference(&file, "left", "Cache", ReferenceKind::TypeOf));
    assert!(has_reference(
        &file,
        "process",
        "Local",
        ReferenceKind::TypeOf
    ));
    assert!(has_reference(
        &file,
        "process",
        "Limit",
        ReferenceKind::TypeOf
    ));
    assert!(
        file.symbols
            .iter()
            .all(|entry| entry.name != "_" && entry.name != "local" && entry.name != "limit")
    );
}

#[test]
fn go_struct_fields_are_contained_field_symbols_with_struct_type_consumers() {
    let file = extract(
        "api/client.go",
        r#"
package api

type Client struct {
    Host    string
    Port    int    `json:"port"`
    timeout int
    X, Y    float64
    *Mutex
    db      *Database
    items   []Item
    inner   struct {
        Nested Leaf
    }
}
"#,
    );

    let client = symbol(&file, "Client");
    assert_eq!(
        names_of_kind(&file, SymbolKind::Field),
        ["Host", "Port", "X", "Y", "db", "inner", "items", "timeout"]
    );
    assert_eq!(
        symbol(&file, "Client::Port").signature.as_deref(),
        Some("Port int")
    );
    assert!(
        symbol(&file, "Client::X")
            .signature
            .as_deref()
            .is_some_and(|sig| sig.contains("float64"))
    );
    assert_eq!(
        symbol(&file, "Client::db").signature.as_deref(),
        Some("db *Database")
    );
    assert!(symbol(&file, "Client::Host").export.exported);
    assert!(!symbol(&file, "Client::timeout").export.exported);
    let port = symbol(&file, "Client::Port");
    assert!(
        file.containments
            .iter()
            .any(|edge| edge.parent == client.id && edge.child == port.id)
    );
    for target in ["Database", "Item", "Leaf"] {
        assert!(
            has_reference(&file, "Client", target, ReferenceKind::TypeOf),
            "{target}"
        );
    }
    assert!(has_reference(
        &file,
        "Client",
        "Mutex",
        ReferenceKind::Extends
    ));
    assert!(!has_reference(
        &file,
        "Client",
        "Mutex",
        ReferenceKind::TypeOf
    ));
    assert!(file.symbols.iter().all(|entry| entry.name != "Nested"));
}

#[test]
fn go_struct_and_interface_embedding_extends_leaf_types() {
    let file = extract(
        "gemma2/model.go",
        r"
package gemma2

type Model struct {
    model.Base
    *model.Pointer
    tokenizer.Tokenizer
    *Options
    Generic[int]
}

type ReadWriter interface {
    io.Reader
    Writer
    ~int | ~string
    Close() error
}
",
    );

    let mut extends = owned_names(&file, "Model", ReferenceKind::Extends);
    extends.sort_unstable();
    assert_eq!(
        extends,
        ["Base", "Generic", "Options", "Pointer", "Tokenizer"]
    );
    let base = reference(&file, "Model", "Base", ReferenceKind::Extends);
    assert_eq!(base.resolution_name.as_deref(), Some("model.Base"));
    let options = reference(&file, "Model", "Options", ReferenceKind::Extends);
    assert!(options.resolution_name.is_none());
    let mut interface = owned_names(&file, "ReadWriter", ReferenceKind::Extends);
    interface.sort_unstable();
    assert_eq!(interface, ["Reader", "Writer"]);
    for embedded in ["Base", "Pointer", "Tokenizer", "Options"] {
        assert!(!has_reference(
            &file,
            "Model",
            embedded,
            ReferenceKind::TypeOf
        ));
    }
    assert_eq!(names_of_kind(&file, SymbolKind::Field), NONE);
}

#[test]
fn go_cgo_calls_name_the_c_function() {
    let file = extract(
        "native/main.go",
        r#"
package main

import "C"

func runIt() {
    n := C.do_thing(42)
    _ = n
    C.do_other()
}

func notCgo(c *Client) {
    c.do_thing()
}
"#,
    );
    let plain = extract(
        "native/plain.go",
        r"
package main

func run() {
    C.do_thing()
}
",
    );

    let calls = owned_names(&file, "runIt", ReferenceKind::Calls);
    assert_eq!(calls, ["do_thing", "do_other"]);
    let cgo = reference(&file, "runIt", "do_thing", ReferenceKind::Calls);
    assert_eq!(cgo.resolution_name.as_deref(), Some("C.do_thing"));
    assert_eq!(
        owned_names(&file, "notCgo", ReferenceKind::Calls),
        ["c.do_thing"]
    );
    assert_eq!(owned_names(&file, "runIt", ReferenceKind::References), NONE);
    assert_eq!(
        owned_names(&plain, "run", ReferenceKind::Calls),
        ["C.do_thing"]
    );
}

#[test]
fn go_composite_literals_instantiate_named_types_only() {
    let file = extract(
        "api/build.go",
        r"
package api

func build() {
    a := Foo{X: 1}
    b := &pkg.Bar{}
    c := Gen[int]{}
    d := []Item{}
    e := map[string]Value{}
    f := [2]Pair{}
    _, _, _, _, _, _ = a, b, c, d, e, f
}
",
    );

    let mut created = owned_names(&file, "build", ReferenceKind::Instantiates);
    created.sort_unstable();
    assert_eq!(created, ["Bar", "Foo", "Gen"]);
    let bar = reference(&file, "build", "Bar", ReferenceKind::Instantiates);
    assert_eq!(bar.resolution_name.as_deref(), Some("pkg.Bar"));
}

#[test]
fn rust_struct_expressions_instantiate_their_type() {
    let file = extract(
        "src/build.rs",
        r"
struct S { a: u8 }

impl S {
    fn new() -> Self {
        Self { a: 0 }
    }
}

fn build() {
    let s = S { a: 1 };
    let p = m::P { b: 2 };
    let _ = (s, p);
}
",
    );

    let mut created = owned_names(&file, "build", ReferenceKind::Instantiates);
    created.sort_unstable();
    assert_eq!(created, ["P", "S"]);
    let qualified = reference(&file, "build", "P", ReferenceKind::Instantiates);
    assert_eq!(qualified.resolution_name.as_deref(), Some("m::P"));
    assert_eq!(
        owned_names(&file, "S::new", ReferenceKind::Instantiates),
        NONE
    );
}

#[test]
fn rust_supertraits_extend_each_named_bound() {
    let file = extract(
        "src/traits.rs",
        r"
pub trait Worker: Clone + Send + for<'a> Fn(&'a u8) + Iterator<Item = u8> + std::fmt::Debug + 'static {}

pub trait Plain {}
",
    );

    let mut supertraits = owned_names(&file, "Worker", ReferenceKind::Extends);
    supertraits.sort_unstable();
    assert_eq!(supertraits, ["Clone", "Debug", "Fn", "Iterator", "Send"]);
    let debug = reference(&file, "Worker", "Debug", ReferenceKind::Extends);
    assert_eq!(debug.resolution_name.as_deref(), Some("std::fmt::Debug"));
    assert_eq!(owned_names(&file, "Plain", ReferenceKind::Extends), NONE);
}

#[test]
fn rust_struct_field_types_are_struct_type_consumers() {
    let file = extract(
        "src/model.rs",
        r"
pub struct K {
    repo: Repo,
    items: Vec<Item>,
}

pub struct Wrapper(pub Inner);
",
    );

    for target in ["Repo", "Vec", "Item"] {
        assert!(
            has_reference(&file, "K", target, ReferenceKind::TypeOf),
            "{target}"
        );
    }
    assert!(has_reference(
        &file,
        "Wrapper",
        "Inner",
        ReferenceKind::TypeOf
    ));
    assert_eq!(names_of_kind(&file, SymbolKind::Field), NONE);
}

#[test]
fn go_pascal_case_reads_reference_package_bindings() {
    let file = extract(
        "api/limits.go",
        r"
package api

var MaxN = 10
var Fallback = MaxN

func limit(n int) int {
    Config = 2
    Shadow := 3
    point := Point{X: MaxN}
    sort.Slice(items, Less)
    Helper()
    if n > MaxN {
        return MaxN
    }
    return pkg.Remote + Shadow + point.X
}
",
    );

    assert_eq!(
        owned_names(&file, "limit", ReferenceKind::References),
        ["MaxN", "Less", "MaxN", "MaxN", "Shadow"]
    );
    assert_eq!(
        owned_names(&file, "Fallback", ReferenceKind::References),
        ["MaxN"]
    );
    assert_eq!(owned_names(&file, "MaxN", ReferenceKind::References), NONE);
}

#[test]
fn rust_constant_shaped_reads_reference_constants_without_duplicates() {
    let file = extract(
        "src/limits.rs",
        r#"
pub const LIMIT: u32 = 3;
pub static mut COUNTER: u32 = 0;

#[cfg(feature = "FLAG_X")]
fn limited(value: u32) -> u32 {
    unsafe { COUNTER = 1; }
    let total = LIMIT + OTHER_X;
    println!("{}", MACRO_X);
    let path = config::PATH_LIMIT;
    let ready = CALLEE_X();
    match value {
        MATCH_X => total,
        _ => total + ready + path,
    }
}
"#,
    );

    let mut reads = owned_names(&file, "limited", ReferenceKind::References);
    reads.sort_unstable();
    assert_eq!(
        reads,
        [
            "LIMIT",
            "MACRO_X",
            "MATCH_X",
            "OTHER_X",
            "config::PATH_LIMIT"
        ]
    );
    assert_eq!(owned_names(&file, "LIMIT", ReferenceKind::References), NONE);
}

#[test]
fn go_multi_name_initializers_are_owned_per_binding() {
    let file = extract(
        "api/pairs.go",
        r"
package api

var A, B = buildA(), buildB()
var _, C = discard(), buildC()
var D, E = pair()
",
    );

    assert_eq!(owned_names(&file, "A", ReferenceKind::Calls), ["buildA"]);
    assert_eq!(owned_names(&file, "B", ReferenceKind::Calls), ["buildB"]);
    assert_eq!(owned_names(&file, "C", ReferenceKind::Calls), ["buildC"]);
    assert_eq!(owned_names(&file, "D", ReferenceKind::Calls), ["pair"]);
    assert_eq!(owned_names(&file, "E", ReferenceKind::Calls), NONE);
    let discard = file
        .references
        .iter()
        .find(|reference| reference.name == "discard")
        .unwrap_or_else(|| panic!("discarded initializer lost its call"));
    assert!(discard.owner.is_none(), "a blank binding owns nothing");
}

#[test]
fn go_multi_name_constants_declare_only_their_names() {
    // The grammar files the commas of `const A, B` under the name field too.
    let file = extract(
        "api/consts.go",
        r"
package api

const A, B = real(1+2i), imag(1+2i)
const C, D Size = 1, 2
",
    );

    assert_eq!(
        names_of_kind(&file, SymbolKind::Constant),
        ["A", "B", "C", "D"]
    );
    assert_eq!(owned_names(&file, "A", ReferenceKind::Calls), ["real"]);
    assert_eq!(owned_names(&file, "B", ReferenceKind::Calls), ["imag"]);
    assert_eq!(owned_names(&file, "C", ReferenceKind::TypeOf), ["Size"]);
    assert_eq!(owned_names(&file, "D", ReferenceKind::TypeOf), ["Size"]);
}

#[test]
fn go_reads_keep_expression_keys_and_skip_parameters() {
    let file = extract(
        "api/keys.go",
        r"
package api

const Limit = 10
const Offset = 1

func shadowed(Limit int) int {
    return consume(Limit)
}

type Worker struct{}

func (Limit *Worker) receiver() {
    use(Limit)
}

func outer() {
    run(func(Offset int) int { return Offset })
}

func keyed() {
    m := map[int]int{Offset: 1}
    a := [...]int{Offset: 2}
    s := Settings{Offset: 3}
    items[Index] = 4
    Config.Field = 5
    _, _, _ = m, a, s
}
",
    );

    assert_eq!(
        owned_names(&file, "shadowed", ReferenceKind::References),
        NONE
    );
    assert_eq!(
        owned_names(&file, "Worker::receiver", ReferenceKind::References),
        NONE
    );
    assert_eq!(owned_names(&file, "outer", ReferenceKind::References), NONE);
    assert_eq!(
        owned_names(&file, "keyed", ReferenceKind::References),
        ["Offset", "Offset", "Index", "Config"]
    );
}

#[test]
fn rust_reads_skip_parameters_and_const_generics() {
    let file = extract(
        "src/shadow.rs",
        r"
const LEN: usize = 3;

fn generic<const LEN: usize>() -> usize {
    LEN
}

#[allow(non_snake_case)]
fn parameter(MAX_X: u8) -> u8 {
    MAX_X
}

fn closure() -> u8 {
    let pick = |CLOSE_X: u8| CLOSE_X;
    pick(1)
}

fn uses() -> usize {
    LEN
}
",
    );

    for owner in ["generic", "parameter", "closure"] {
        assert_eq!(
            owned_names(&file, owner, ReferenceKind::References),
            NONE,
            "{owner}"
        );
    }
    assert_eq!(
        owned_names(&file, "uses", ReferenceKind::References),
        ["LEN"]
    );
}

#[test]
fn rust_turbofish_struct_expressions_are_one_instantiation() {
    let file = extract(
        "src/turbofish.rs",
        r"
fn build() {
    let s = m::S::<u8> { a: 1 };
    let t = T::<u8> { b: 2 };
    let _ = (s, t);
}
",
    );

    let mut created = owned_names(&file, "build", ReferenceKind::Instantiates);
    created.sort_unstable();
    assert_eq!(created, ["S", "T"]);
    let qualified = reference(&file, "build", "S", ReferenceKind::Instantiates);
    assert_eq!(qualified.resolution_name.as_deref(), Some("m::S"));
    assert_eq!(owned_names(&file, "build", ReferenceKind::References), NONE);
}

#[test]
fn declared_types_keep_their_qualifier_once_per_occurrence() {
    let python = extract(
        "app/box.py",
        r"
class Box:
    repo: models.Repo
    items: list[models.Item]
",
    );
    let go = extract(
        "api/k.go",
        r"
package api

type K struct {
    db   *store.DB
    X, Y Point
}
",
    );
    let rust = extract(
        "src/k.rs",
        r"
struct K {
    repo: store::Repo,
    rows: Box<dyn Iterator<Item = Row>>,
}
",
    );

    let repo = reference(&python, "Box", "Repo", ReferenceKind::TypeOf);
    assert_eq!(repo.resolution_name.as_deref(), Some("models.Repo"));
    let item = reference(&python, "Box", "Item", ReferenceKind::TypeOf);
    assert_eq!(item.resolution_name.as_deref(), Some("models.Item"));
    assert!(!has_reference(
        &python,
        "Box",
        "models",
        ReferenceKind::TypeOf
    ));
    let database = reference(&go, "K", "DB", ReferenceKind::TypeOf);
    assert_eq!(database.resolution_name.as_deref(), Some("store.DB"));
    assert_eq!(reference_kinds(&go, "K", "Point"), [ReferenceKind::TypeOf]);
    let stored = reference(&rust, "K", "Repo", ReferenceKind::TypeOf);
    assert_eq!(stored.resolution_name.as_deref(), Some("store::Repo"));
    let mut rust_types = owned_names(&rust, "K", ReferenceKind::TypeOf);
    rust_types.sort_unstable();
    assert_eq!(rust_types, ["Box", "Iterator", "Repo", "Row"]);
}

#[test]
fn python_rebinds_keep_values_and_annotations_on_the_declared_variable() {
    let file = extract(
        "app/registry.py",
        r"
if flag:
    registry = make_a()
else:
    registry: OtherRegistry = make_b()

first = second = build()
",
    );

    assert_eq!(
        names_of_kind(&file, SymbolKind::Variable),
        ["first", "registry"]
    );
    assert_eq!(
        owned_names(&file, "registry", ReferenceKind::Calls),
        ["make_a", "make_b"]
    );
    assert!(has_reference(
        &file,
        "registry",
        "OtherRegistry",
        ReferenceKind::TypeOf
    ));
    assert_eq!(owned_names(&file, "first", ReferenceKind::Calls), ["build"]);
}

#[test]
fn python_decorators_skip_computed_receivers_and_find_stacked_staticmethod() {
    let file = extract(
        "app/stacked.py",
        r#"
class K:
    @trace
    @staticmethod
    def build():
        pass

    @factory("secret").route
    def computed(self):
        pass

    @registry["key"].decorate
    def indexed(self):
        pass
"#,
    );

    assert!(symbol(&file, "K::build").execution.static_member);
    assert_eq!(decorators_of(&file, "K::build"), ["trace", "staticmethod"]);
    assert_eq!(decorators_of(&file, "K::computed"), NONE);
    assert_eq!(decorators_of(&file, "K::indexed"), NONE);
    assert!(file.references.iter().all(|reference| {
        !reference.name.contains("secret")
            && !reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.contains("secret") || name.contains("key"))
    }));
}

#[test]
fn rust_direct_fn_supertrait_extends_its_trait() {
    let file = extract("src/handler.rs", "trait Handler: Fn(u8) -> u8 {}\n");

    assert_eq!(
        owned_names(&file, "Handler", ReferenceKind::Extends),
        ["Fn"]
    );
}

#[test]
fn go_field_signatures_never_carry_tags_or_literals() {
    let file = extract(
        "api/tagged.go",
        r#"
package api

type K struct {
    Inner struct {
        A int `json:"a"`
    }
    Plain int `json:"plain"`
}
"#,
    );

    assert!(
        symbol(&file, "K::Inner")
            .signature
            .as_deref()
            .is_none_or(|signature| !signature.contains("json"))
    );
    assert_eq!(
        symbol(&file, "K::Plain").signature.as_deref(),
        Some("Plain int")
    );
}

#[test]
fn field_and_binding_types_keep_the_values_they_read() {
    let rust = extract(
        "src/sized.rs",
        r"
const LEN: usize = 4;
struct S {
    bytes: [u8; crate::LEN],
    more: [u8; LEN],
}
",
    );
    let go = extract(
        "api/sized.go",
        r"
package api

const Size = 4

type Client struct {
    Bytes [Size]byte
}

var buffer [Size]byte
",
    );

    let mut struct_reads = owned_names(&rust, "S", ReferenceKind::References);
    struct_reads.sort_unstable();
    assert_eq!(struct_reads, ["LEN", "crate::LEN"]);
    assert_eq!(
        owned_names(&go, "Client", ReferenceKind::References),
        ["Size"]
    );
    assert_eq!(
        owned_names(&go, "buffer", ReferenceKind::References),
        ["Size"]
    );
}

#[test]
fn go_initializer_comments_do_not_shift_ownership() {
    let file = extract(
        "api/commented.go",
        r"
package api

var A, B = buildA(), /* explanation */ buildB()
",
    );

    assert_eq!(owned_names(&file, "A", ReferenceKind::Calls), ["buildA"]);
    assert_eq!(owned_names(&file, "B", ReferenceKind::Calls), ["buildB"]);
}

#[test]
fn raw_and_spaced_paths_keep_a_plain_lookup_name() {
    let rust = extract(
        "src/raw.rs",
        r"
struct W {
    value: r#type::S,
}

fn build() {
    let _ = r#type::S { n: 0 };
}
",
    );
    let python = extract("app/spaced.py", "class Box:\n    repo: models . Repo\n");

    let field = reference(&rust, "W", "S", ReferenceKind::TypeOf);
    assert_eq!(field.resolution_name.as_deref(), Some("r#type::S"));
    let created = reference(&rust, "build", "S", ReferenceKind::Instantiates);
    assert_eq!(created.resolution_name.as_deref(), Some("r#type::S"));
    let spaced = reference(&python, "Box", "Repo", ReferenceKind::TypeOf);
    assert_eq!(spaced.resolution_name.as_deref(), Some("models.Repo"));
}

#[test]
fn rust_pattern_bindings_and_labels_are_not_constant_reads() {
    let file = extract(
        "src/patterns.rs",
        r"
const OUTER: u8 = 1;

#[allow(non_snake_case)]
fn wrapped(&PARAM_X: &u8) -> u8 {
    PARAM_X
}

#[allow(non_snake_case)]
fn bindings() -> u8 {
    'OUTER: loop {
        break 'OUTER;
    }
    let ref DECL_X = 1;
    let pick = |(CLOSE_X,): (u8,)| CLOSE_X;
    pick((1,))
}

fn constant_pattern(value: u8) -> bool {
    matches_outer(value) || match value {
        OUTER => true,
        _ => false,
    }
}
",
    );

    assert_eq!(
        owned_names(&file, "wrapped", ReferenceKind::References),
        NONE
    );
    assert_eq!(
        owned_names(&file, "bindings", ReferenceKind::References),
        NONE
    );
    assert_eq!(
        owned_names(&file, "constant_pattern", ReferenceKind::References),
        ["OUTER"]
    );
}

#[test]
fn python_annotation_values_are_not_types() {
    let file = extract(
        "app/annotated.py",
        r#"
import typing
from typing import Annotated, Literal
from typing_extensions import Annotated as A

class Box:
    value: Annotated[Repo, Tag()]
    kind: Literal[VALUE, "x"]
    aliased: A[Stored, Meta]
    qualified: typing.Annotated[ # explanation
        Commented, Marker]
"#,
    );
    let local = extract(
        "app/local_literal.py",
        "class Literal(Generic[T]):\n    pass\n\nclass GenericBox:\n    value: Literal[Repo]\n",
    );

    let mut types = owned_names(&file, "Box", ReferenceKind::TypeOf);
    types.sort_unstable();
    assert_eq!(
        types,
        [
            "A",
            "Annotated",
            "Annotated",
            "Commented",
            "Literal",
            "Repo",
            "Stored"
        ]
    );
    assert!(has_reference(&file, "Box", "Tag", ReferenceKind::Calls));
    let mut local_types = owned_names(&local, "GenericBox", ReferenceKind::TypeOf);
    local_types.sort_unstable();
    assert_eq!(local_types, ["Literal", "Repo"]);
}

#[test]
fn qualified_paths_never_carry_literals_or_comments() {
    let file = extract(
        "app/paths.py",
        "class Box:\n    real: 8675309 .real\n    repo: (models # explanation\n           .Repo)\n",
    );

    let repo = reference(&file, "Box", "Repo", ReferenceKind::TypeOf);
    assert_eq!(repo.resolution_name.as_deref(), Some("models.Repo"));
    assert!(file.references.iter().all(|reference| {
        !reference.name.contains("8675309")
            && !reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.contains("8675309") || name.contains('#'))
    }));
    assert!(!has_reference(&file, "Box", "real", ReferenceKind::TypeOf));
}

#[test]
fn rust_explicit_and_deep_bindings_are_not_constant_reads() {
    let mut pattern = "DEEP_X".to_owned();
    let mut value = "0u8".to_owned();
    for _ in 0..40 {
        pattern = format!("({pattern},)");
        value = format!("({value},)");
    }
    let deep = extract(
        "src/deep.rs",
        &format!("fn deep() {{ let {pattern} = {value}; }}\n"),
    );
    let explicit = extract(
        "src/explicit.rs",
        r"
fn explicit(n: u8) {
    match n {
        ref MATCH_X => (),
    }
    if let Some(ref MAYBE_X) = Some(n) {}
    if let BOUND_X @ 1..=9 = n {}
}
",
    );

    assert_eq!(owned_names(&deep, "deep", ReferenceKind::References), NONE);
    assert_eq!(
        owned_names(&explicit, "explicit", ReferenceKind::References),
        NONE
    );
}

#[test]
fn go_pointer_elided_literals_keep_expression_keys() {
    let file = extract(
        "api/pointers.go",
        r"
package api

func pointers() {
    _ = []*[2]int{{Inner: 3}}
    _ = map[int]*map[int]int{0: {Inner: 3}}
    _ = []*Point{{Field: 1}}
}
",
    );

    assert_eq!(
        owned_names(&file, "pointers", ReferenceKind::References),
        ["Inner", "Inner"]
    );
}

#[test]
fn go_elided_nested_literals_keep_expression_keys() {
    let file = extract(
        "api/nested.go",
        r"
package api

func nested() {
    _ = map[int]map[int]int{Outer: {Inner: 3}}
    _ = []Point{{Field: 1}}
}
",
    );

    assert_eq!(
        owned_names(&file, "nested", ReferenceKind::References),
        ["Outer", "Inner"]
    );
}

#[test]
fn go_import_aliased_as_c_is_not_cgo() {
    let file = extract(
        "api/alias.go",
        r#"
package api

import C "strings"

func check() bool {
    return C.Contains("a", "b")
}
"#,
    );

    assert_eq!(
        owned_names(&file, "check", ReferenceKind::Calls),
        ["C.Contains"]
    );
}

#[test]
fn python_qualified_staticmethod_lookalike_is_not_static() {
    let file = extract(
        "app/lookalike.py",
        r"
class C:
    @ns.staticmethod
    def method(self):
        return 1
",
    );

    assert!(!symbol(&file, "C::method").execution.static_member);
    assert_eq!(decorators_of(&file, "C::method"), ["staticmethod"]);
}

#[test]
fn python_called_staticmethod_is_not_static() {
    let file = extract(
        "app/called.py",
        r"
class C:
    @staticmethod(identity)
    def wrapped(self):
        return 1

    @staticmethod
    def plain():
        return 2
",
    );

    assert!(!symbol(&file, "C::wrapped").execution.static_member);
    assert!(symbol(&file, "C::plain").execution.static_member);
}

#[test]
fn go_field_signatures_never_copy_comments() {
    let file = extract(
        "api/commented.go",
        r"
package api

type Config struct {
    Buffer [/* secret_canary */ Size]byte
    Inner struct {
        Thing int // password hunter2
    }
    Plain Thing
}
",
    );

    for field in ["Config::Buffer", "Config::Inner"] {
        assert!(
            symbol(&file, field)
                .signature
                .as_deref()
                .is_none_or(|signature| {
                    !signature.contains("secret_canary") && !signature.contains("hunter2")
                }),
            "{field} copied a comment"
        );
    }
    assert_eq!(
        symbol(&file, "Config::Plain").signature.as_deref(),
        Some("Plain Thing")
    );
}

#[test]
fn deep_declared_types_and_interface_constraints_keep_existing_facts() {
    let deep_type = format!("{}u8{}", "[".repeat(66), "; 1]".repeat(66));
    let rust = extract(
        "src/deep_type.rs",
        &format!(
            "fn target() {{}}\nfn caller() {{ target(); }}\nstruct S {{ value: {deep_type} }}\n"
        ),
    );
    let go = extract(
        "api/constraint.go",
        "package api\n\ntype Sized interface {\n    ~[len(\"hello\")]byte\n}\n",
    );

    assert_eq!(
        owned_names(&rust, "caller", ReferenceKind::Calls),
        ["target"]
    );
    assert_eq!(owned_names(&go, "Sized", ReferenceKind::Calls), ["len"]);
}

#[test]
fn rust_outer_attributes_decorate_only_the_item_they_precede() {
    let file = extract(
        "src/main.rs",
        r#"#![allow(unused)]
#[derive(Debug, Clone)]
pub struct Order;

#[get("/a")]
#[post("/b")]
async fn multi() {}

#[tauri::command]
// a comment between the attribute and its item
fn bridge() {}

#[cfg(test)]
mod tests {
    #[test]
    fn works() {}
}

#[allow(dead_code)]
const LIMIT: u32 = 1;

#[cfg(feature = "x")]
mod external;

fn plain() {}
"#,
    );

    assert_eq!(decorators_of(&file, "Order"), ["derive"]);
    assert_eq!(decorators_of(&file, "multi"), ["get", "post"]);
    let command = reference(&file, "bridge", "command", ReferenceKind::Decorates);
    assert_eq!(command.resolution_name.as_deref(), Some("tauri::command"));
    assert_eq!(decorators_of(&file, "tests"), ["cfg"]);
    assert_eq!(decorators_of(&file, "tests::works"), ["test"]);
    assert_eq!(decorators_of(&file, "LIMIT"), ["allow"]);
    assert_eq!(decorators_of(&file, "plain"), NONE);
    assert_eq!(
        file.references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Decorates)
            .count(),
        7,
        "the inner attribute and the out-of-line module's attribute decorate nothing"
    );
}

#[test]
fn rust_same_file_expression_macros_read_constants_in_their_arguments() {
    let file = extract(
        "src/lib.rs",
        r"
const MAX_ITEMS: usize = 64;

fn before(n: usize) -> bool {
    ensure!(n < MAX_ITEMS)
}

macro_rules! ensure {
    ($cond:expr) => { $cond };
    ($cond:expr, $($rest:expr),+) => { $cond };
}

macro_rules! keyed {
    (key = $value:expr) => { $value };
}

macro_rules! named {
    ($name:ident) => { 0 };
}

macro_rules! vals {
    ($($v:literal) MAX_ITEMS *) => { 0 };
}

macro_rules! sum {
    ($($v:expr);+) => { 0 };
}

fn after(n: usize) -> bool {
    ensure!(n < MAX_ITEMS, MAX_ITEMS > 1)
}

fn dsl() -> usize {
    keyed!(key = MAX_ITEMS) + named!(MAX_ITEMS) + vals!(1 MAX_ITEMS 2)
}

fn separated() -> usize {
    sum!(MAX_ITEMS; 2)
}

macro_rules! ensure {
    ($name:ident) => { true };
}

fn shadowed() -> bool {
    ensure!(MAX_ITEMS)
}
",
    );

    assert_eq!(
        owned_names(&file, "before", ReferenceKind::References),
        NONE,
        "a macro_rules! defined later is not in scope"
    );
    assert_eq!(
        owned_names(&file, "after", ReferenceKind::References),
        ["MAX_ITEMS", "MAX_ITEMS"]
    );
    assert_eq!(
        owned_names(&file, "dsl", ReferenceKind::References),
        NONE,
        "a key, an ident fragment, or a word separator is not an expression"
    );
    assert_eq!(
        owned_names(&file, "separated", ReferenceKind::References),
        ["MAX_ITEMS"],
        "a punctuation separator keeps the expression reading"
    );
    assert_eq!(
        owned_names(&file, "shadowed", ReferenceKind::References),
        NONE,
        "a later definition that binds names shadows the expression macro"
    );
}

#[test]
fn rust_expression_macro_scope_ends_with_its_block_and_skips_bound_names() {
    let file = extract(
        "src/scoped.rs",
        r#"
const MAX_ITEMS: usize = 64;

macro_rules! check {
    ($e:expr) => { $e };
}

fn inner() -> bool {
    macro_rules! check {
        ($name:ident) => { true };
    }
    check!(MAX_ITEMS)
}

fn after() -> bool {
    check!(MAX_ITEMS > 1)
}

fn generic<const WIDTH: usize>(LIMIT: usize) -> bool {
    assert!(WIDTH > 0, "{WIDTH} {LIMIT}");
    check!(WIDTH > LIMIT)
}

#[macro_use]
mod exported {
    macro_rules! check {
        ($name:ident) => { true };
    }
}

mod private {
    macro_rules! probe {
        ($name:ident) => { true };
    }
}

macro_rules! probe {
    ($e:expr) => { $e };
}

fn after_modules() -> bool {
    check!(MAX_ITEMS) && probe!(MAX_ITEMS > 2)
}
"#,
    );

    assert_eq!(
        owned_names(&file, "inner", ReferenceKind::References),
        NONE,
        "the block-scoped definition binds names"
    );
    assert_eq!(
        owned_names(&file, "after", ReferenceKind::References),
        ["MAX_ITEMS"],
        "the outer expression macro is back in scope after the block"
    );
    assert_eq!(
        owned_names(&file, "generic", ReferenceKind::References),
        NONE,
        "parameters and const generics are not constant reads inside macros"
    );
    assert_eq!(
        owned_names(&file, "after_modules", ReferenceKind::References),
        ["MAX_ITEMS"],
        "a #[macro_use] module's definition shadows past the module; a plain module's does not"
    );
}

#[test]
fn rust_outer_parameters_do_not_hide_constant_reads_in_nested_items() {
    let file = extract(
        "src/nested.rs",
        r#"
const WIDTH: usize = 8;

fn outer(WIDTH: usize) {
    mod m {
        fn inner() {
            assert!(WIDTH > 0, "{WIDTH}");
            let _x = WIDTH;
        }
    }
    fn nested() -> usize {
        assert_eq!(WIDTH, 8);
        WIDTH
    }
    let close = || {
        assert!(WIDTH > 1);
        WIDTH
    };
}

struct Grid<const WIDTH: usize>;

impl<const WIDTH: usize> Grid<WIDTH> {
    fn cells(&self) -> usize {
        assert!(WIDTH > 0);
        WIDTH
    }
}
"#,
    );

    assert_eq!(
        owned_names(&file, "outer::m::inner", ReferenceKind::References),
        ["WIDTH", "WIDTH", "WIDTH"],
        "an item inside a function body cannot see the function's parameters"
    );
    assert_eq!(
        owned_names(&file, "outer::nested", ReferenceKind::References),
        ["WIDTH", "WIDTH"],
        "a nested function cannot see the enclosing function's parameters"
    );
    assert_eq!(
        owned_names(&file, "outer", ReferenceKind::References),
        NONE,
        "a closure sees its enclosing function's parameters"
    );
    assert_eq!(
        owned_names(&file, "Grid::cells", ReferenceKind::References),
        NONE,
        "a method sees its impl's const generics"
    );
}

#[test]
fn go_literals_of_file_declared_container_types_keep_expression_keys() {
    let file = extract(
        "api/tables.go",
        r#"
package api

const Key = "k"
const Pos = 1

type M map[string]int
type N M
type A = [4]int
type P (map[string]int)
type G[K comparable, V any] map[K]V
type I G[string, int]
type S struct{ Key string }
type T map[string]int

func build() {
    m := M{Key: 1}
    n := N{Key: 2}
    a := A{Pos: 3}
    p := P{Key: 4}
    i := I{Key: 5}
    s := S{Key: "v"}
    r := pkg.Remote{Key: "w"}
    _, _, _, _, _, _, _ = m, n, a, p, i, s, r
}

func shadow() {
    type T struct{ Key int }
    _ = T{Key: 1}
}
"#,
    );

    assert_eq!(
        owned_names(&file, "build", ReferenceKind::FieldAccess),
        ["Key", "Key"],
        "only the struct and the unknown remote type have field keys"
    );
    assert_eq!(
        owned_names(&file, "build", ReferenceKind::References),
        ["Key", "Key", "Pos", "Key", "Key"]
    );
    assert_eq!(
        owned_names(&file, "shadow", ReferenceKind::FieldAccess),
        ["Key"],
        "a function-local struct may shadow the package-level map type"
    );
}

#[test]
fn go_named_literal_field_keys_are_field_accesses() {
    let file = extract(
        "api/user.go",
        r#"
package api

func build(email string, k int) {
    p := &Profile{Bio: "new"}
    u := &User{Email: email, Profile: p}
    q := pkg.Remote{Name: email}
    m := map[int]string{k: "v"}
    s := Pair{1, 2}
    _, _, _, _, _ = p, u, q, m, s
}
"#,
    );

    assert_eq!(
        owned_names(&file, "build", ReferenceKind::FieldAccess),
        ["Bio", "Email", "Profile", "Name"]
    );
    assert_eq!(owned_names(&file, "build", ReferenceKind::References), NONE);
}

#[test]
fn go_and_python_member_calls_written_with_layout_use_their_lookup_name() {
    let go = extract(
        "api/routes.go",
        "package api\n\nfunc register(r Router) {\n    r. POST(\"/users\", h)\n    r./* why */GET(\"/x\", h)\n    r.Delete(\"/y\", h)\n}\n",
    );
    let python = extract(
        "app/jobs.py",
        "def run(queue):\n    queue . push(1)\n    queue.pop()\n",
    );

    assert_eq!(
        owned_names(&go, "register", ReferenceKind::Calls),
        ["r.POST", "r.GET", "r.Delete"]
    );
    assert_eq!(
        owned_names(&python, "run", ReferenceKind::Calls),
        ["queue.push", "queue.pop"]
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = match SourceLimits::new(SOURCE_LIMIT_BYTES) {
        Ok(limits) => limits,
        Err(error) => panic!("source limits are invalid: {error}"),
    };
    let snapshot = match SourceSnapshot::from_bytes(path, source.as_bytes(), limits) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("snapshot failed: {error}"),
    };
    let mut extractor = match NativeExtractor::new(snapshot.language()) {
        Ok(extractor) => extractor,
        Err(error) => panic!("grammar failed: {error}"),
    };
    match extractor.extract(&snapshot) {
        Ok(file) => file,
        Err(error) => panic!("extraction failed: {error}"),
    }
}

fn symbol<'a>(file: &'a ExtractedFile, qualified_name: &str) -> &'a ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("missing symbol {qualified_name}"))
}

fn names_of_kind(file: &ExtractedFile, kind: SymbolKind) -> Vec<&str> {
    let mut names = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

fn owned_names<'a>(file: &'a ExtractedFile, owner: &str, kind: ReferenceKind) -> Vec<&'a str> {
    let owner = symbol(file, owner);
    file.references
        .iter()
        .filter(|reference| reference.kind == kind && reference.owner.as_ref() == Some(&owner.id))
        .map(|reference| reference.name.as_str())
        .collect()
}

fn decorators_of<'a>(file: &'a ExtractedFile, owner: &str) -> Vec<&'a str> {
    owned_names(file, owner, ReferenceKind::Decorates)
}

fn reference_kinds(file: &ExtractedFile, owner: &str, name: &str) -> Vec<ReferenceKind> {
    let owner = symbol(file, owner);
    file.references
        .iter()
        .filter(|reference| reference.name == name && reference.owner.as_ref() == Some(&owner.id))
        .map(|reference| reference.kind)
        .collect()
}

fn has_reference(file: &ExtractedFile, owner: &str, name: &str, kind: ReferenceKind) -> bool {
    reference_kinds(file, owner, name).contains(&kind)
}

fn reference<'a>(
    file: &'a ExtractedFile,
    owner: &str,
    name: &str,
    kind: ReferenceKind,
) -> &'a ExtractedReference {
    let owner_id = &symbol(file, owner).id;
    file.references
        .iter()
        .find(|reference| {
            reference.name == name
                && reference.kind == kind
                && reference.owner.as_ref() == Some(owner_id)
        })
        .unwrap_or_else(|| panic!("missing {kind:?} reference {owner} -> {name}"))
}
