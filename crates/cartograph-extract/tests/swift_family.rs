//! Swift extraction contracts ported from the v1 `swift` extractor tests.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn swift_extensions_and_protocol_requirements_keep_contextual_visibility() {
    let extracted = extract(
        "Sources/Visibility.swift",
        r"public struct S { func ordinary() {} }
public extension S {
    func ping() {}
    private func hidden() {}
    internal func local() {}
    private(set) var value: Int { get { 1 } set {} }
    struct Nested { func nestedDefault() {} }
}
public extension External { func externalPing() {} }
private extension S { func privatePing() {} }
extension S { func defaultPing() {} }
public protocol P {
    func requiredPing()
    var requiredValue: Int { get }
    associatedtype Element
}
internal protocol Q { func internalRequirement() }",
    );
    for name in [
        "ping",
        "value",
        "Nested",
        "externalPing",
        "requiredPing",
        "requiredValue",
        "Element",
    ] {
        let declared = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(declared.visibility, Some(Visibility::Public), "{name}");
        assert!(declared.export.exported, "{name}");
    }
    for (name, visibility) in [
        ("ordinary", Visibility::Internal),
        ("hidden", Visibility::Private),
        ("local", Visibility::Internal),
        ("nestedDefault", Visibility::Internal),
        ("privatePing", Visibility::Private),
        ("defaultPing", Visibility::Internal),
        ("internalRequirement", Visibility::Internal),
    ] {
        let declared = symbol(&extracted, SymbolKind::Method, name);
        assert_eq!(declared.visibility, Some(visibility), "{name}");
        assert!(!declared.export.exported, "{name}");
    }
}

const MEMBERS_SAMPLE: &str = r#"import UIKit

public struct Point: Equatable {
    let x: Int
    var y: Int = 0
    private(set) var z: Double
    static let origin = Point(x: 0, y: 0, z: 0)
    var computed: Int { return x + y }
    func distance(to other: Point) -> Double { return 0 }
    static func make() -> Point { origin }
    mutating func reset() async throws {}
}

enum Color: String {
    case red, green
    case blue = "sk_live_swift_enum_secret"
    case custom(Int)
    func describe() -> String { "c" }
}

public class ViewController: UIViewController, UITableViewDelegate {
    private let repo: Repo
    fileprivate var items: [Item] = []
    override func viewDidLoad() {
        super.viewDidLoad()
        repo.load(items)
    }
    class func shared() -> ViewController { ViewController(repo: Repo()) }
}

protocol ProfileDelegate: AnyObject {
    var name: String { get }
    func didUpdate(_ profile: Profile)
}

extension Point: CustomStringConvertible {
    func scaled(by factor: Double) -> Point { self }
}

actor Counter {
    var count = 0
    func increment() {}
}

let topConst = 1
var topVar: String = "a"

func greet(name: String, item: Item) -> Out {
    let local = 3
    return Out()
}

public func fetch(_ request: Request) async throws -> [Response] { [] }
"#;

#[test]
fn swift_v1_declarations_keep_class_struct_protocol_and_function_kinds() {
    let network = extract(
        "Sources/NetworkManager.swift",
        r"
public class NetworkManager {
    private let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func fetchData(from url: URL) async throws -> Data {
        let (data, _) = try await session.data(from: url)
        return data
    }
}
",
    );
    let class = symbol(&network, SymbolKind::Class, "NetworkManager");
    assert_eq!(class.visibility, Some(Visibility::Public));
    let fetch = symbol(&network, SymbolKind::Method, "fetchData");
    assert!(fetch.execution.async_symbol);
    assert_eq!(fetch.visibility, Some(Visibility::Public));
    symbol(&network, SymbolKind::Field, "session");

    let utils = extract(
        "Sources/utils.swift",
        r#"
func calculateSum(_ numbers: [Int]) -> Int {
    return numbers.reduce(0, +)
}

public func formatCurrency(amount: Double) -> String {
    return String(format: "$%.2f", amount)
}
"#,
    );
    symbol(&utils, SymbolKind::Function, "calculateSum");
    symbol(&utils, SymbolKind::Function, "formatCurrency");

    let user = extract(
        "Sources/User.swift",
        r"
public struct User {
    let id: UUID
    var name: String
    var email: String

    func displayName() -> String {
        return name
    }
}
",
    );
    let user_struct = symbol(&user, SymbolKind::Struct, "User");
    assert!(
        symbols_named(&user, SymbolKind::Class, "User").is_empty(),
        "a struct must not be classified as a class"
    );
    for field in ["id", "name", "email"] {
        let field = symbol(&user, SymbolKind::Field, field);
        assert_eq!(field.visibility, Some(Visibility::Internal));
        assert!(contains(&user, &user_struct.id, &field.id));
    }
    assert_eq!(
        symbol(&user, SymbolKind::Method, "displayName").qualified_name,
        "User::displayName"
    );

    let repository = extract(
        "Sources/Repository.swift",
        r"
public protocol Repository {
    associatedtype Entity

    func find(id: String) async throws -> Entity?
    func save(_ entity: Entity) async throws
}
",
    );
    symbol(&repository, SymbolKind::Interface, "Repository");
    let find = symbol(&repository, SymbolKind::Method, "find");
    assert!(find.implementation.declaration_only);
}

#[test]
fn swift_inheritance_and_protocol_conformance_stay_role_resolved() {
    let extracted = extract(
        "Sources/Inheritance.swift",
        r"
class DataRequest: Request {
    func validate() {}
}

class UploadRequest: DataRequest, Sendable {
    func upload() {}
}

enum AFError: Error {
    case invalidURL
}

struct HTTPMethod: RawRepresentable {
    let rawValue: String
}

protocol UploadConvertible: URLRequestConvertible {
    func asURLRequest() throws -> URLRequest
}
",
    );
    for (owner, kind, base) in [
        ("DataRequest", SymbolKind::Class, "Request"),
        ("UploadRequest", SymbolKind::Class, "DataRequest"),
        ("UploadRequest", SymbolKind::Class, "Sendable"),
        ("AFError", SymbolKind::Enum, "Error"),
        ("HTTPMethod", SymbolKind::Struct, "RawRepresentable"),
        (
            "UploadConvertible",
            SymbolKind::Interface,
            "URLRequestConvertible",
        ),
    ] {
        let owner = symbol(&extracted, kind, owner);
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Inherits, base, Some(owner))
            ),
            "missing {} inherits {base}: {:?}",
            owner.name,
            extracted.references
        );
    }
    symbol(&extracted, SymbolKind::EnumMember, "invalidURL");
}

#[test]
fn swift_members_get_v1_kinds_visibility_static_and_async_flags() {
    let extracted = extract("Sources/Members.swift", MEMBERS_SAMPLE);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);

    let point = symbol(&extracted, SymbolKind::Struct, "Point");
    assert_eq!(point.visibility, Some(Visibility::Public));
    assert!(point.export.exported);
    assert!(
        !point.execution.static_member && !point.execution.async_symbol,
        "a type containing static/async members is neither static nor async"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "x").visibility,
        Some(Visibility::Internal)
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "z").visibility,
        Some(Visibility::Internal),
        "private(set) only restricts the setter"
    );
    assert!(
        symbol(&extracted, SymbolKind::Field, "origin")
            .execution
            .static_member
    );
    assert!(
        symbol(&extracted, SymbolKind::Method, "make")
            .execution
            .static_member
    );
    assert!(
        symbol(&extracted, SymbolKind::Method, "shared")
            .execution
            .static_member
    );
    assert!(
        !symbol(&extracted, SymbolKind::Method, "distance")
            .execution
            .static_member
    );
    let reset = symbol(&extracted, SymbolKind::Method, "reset");
    assert!(reset.execution.async_symbol && !reset.execution.static_member);
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "repo").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "items").visibility,
        Some(Visibility::Private)
    );
    symbol(&extracted, SymbolKind::Class, "ViewController");
    symbol(&extracted, SymbolKind::Class, "Counter");
    symbol(&extracted, SymbolKind::Field, "count");
    symbol(&extracted, SymbolKind::Method, "increment");
    let fetch = symbol(&extracted, SymbolKind::Function, "fetch");
    assert!(fetch.execution.async_symbol);
    assert_eq!(fetch.visibility, Some(Visibility::Public));
    for method in [
        "distance",
        "make",
        "reset",
        "describe",
        "viewDidLoad",
        "shared",
    ] {
        assert!(
            symbols_named(&extracted, SymbolKind::Function, method).is_empty(),
            "type member {method} must be a method"
        );
    }
}

#[test]
fn swift_enums_cases_properties_and_extensions_follow_v1_shapes() {
    let extracted = extract("Sources/Members.swift", MEMBERS_SAMPLE);
    let color = symbol(&extracted, SymbolKind::Enum, "Color");
    for case in ["red", "green", "blue", "custom"] {
        let member = symbol(&extracted, SymbolKind::EnumMember, case);
        assert_eq!(member.qualified_name, format!("Color::{case}"));
        assert!(contains(&extracted, &color.id, &member.id));
    }
    assert!(
        !format!("{extracted:?}").contains("sk_live_swift_enum_secret"),
        "an enum raw value literal leaked"
    );

    symbol(&extracted, SymbolKind::Constant, "topConst");
    symbol(&extracted, SymbolKind::Variable, "topVar");
    assert!(
        symbols_named(&extracted, SymbolKind::Field, "computed").is_empty(),
        "a computed property is not a stored field"
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.name != "local"),
        "function locals are not declarations"
    );
    let requirement = symbol(&extracted, SymbolKind::Property, "name");
    assert_eq!(requirement.qualified_name, "ProfileDelegate::name");
    assert!(requirement.implementation.declaration_only);
    assert!(
        symbols_named(&extracted, SymbolKind::Interface, "name").is_empty(),
        "a protocol property requirement is not an interface"
    );

    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "Point" && symbol.kind != SymbolKind::Component)
            .count(),
        1,
        "an extension reopens its same-file type"
    );
    let scaled = symbol(&extracted, SymbolKind::Method, "scaled");
    assert_eq!(scaled.qualified_name, "Point::scaled");
    let point = symbol(&extracted, SymbolKind::Struct, "Point");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(
            ReferenceKind::Inherits,
            "CustomStringConvertible",
            Some(point)
        )
    ));
}

#[test]
fn swift_parameter_return_and_field_types_emit_type_references() {
    let extracted = extract("Sources/Members.swift", MEMBERS_SAMPLE);
    let greet = symbol(&extracted, SymbolKind::Function, "greet");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Item", Some(greet))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Returns, "Out", Some(greet))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "Out", Some(greet))
    ));
    let distance = symbol(&extracted, SymbolKind::Method, "distance");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Point", Some(distance))
    ));
    let fetch = symbol(&extracted, SymbolKind::Function, "fetch");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Request", Some(fetch))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Returns, "Response", Some(fetch))
    ));
    let repo = symbol(&extracted, SymbolKind::Field, "repo");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Repo", Some(repo))
    ));
    let items = symbol(&extracted, SymbolKind::Field, "items");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Item", Some(items))
    ));
    for builtin in ["String", "Int", "Double"] {
        assert!(
            !extracted.references.iter().any(|reference| {
                matches!(
                    reference.kind,
                    ReferenceKind::TypeOf | ReferenceKind::Returns
                ) && reference.name == builtin
            }),
            "builtin type {builtin} must not become a type reference"
        );
    }
    assert!(
        !has_reference(
            &extracted,
            ReferenceQuery::new(ReferenceKind::Returns, "Double", None)
        ),
        "a builtin return type is filtered"
    );
}

#[test]
fn swift_generic_parameters_and_metatypes_are_not_type_references() {
    let extracted = extract(
        "Sources/Generic.swift",
        r"
struct Model {}
func load<Model: Decodable>(_ type: Model.Type, from feed: Feed) -> [Model] { [] }
struct Container<Element> {
    var items: [Element]
    var owner: Owner
    func first<Key>(by key: Key) -> Element? { nil }
}
",
    );
    let load = symbol(&extracted, SymbolKind::Function, "load");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Feed", Some(load))
    ));
    for shadowed in ["Model", "Type", "Element", "Key"] {
        assert!(
            !extracted.references.iter().any(|reference| {
                matches!(
                    reference.kind,
                    ReferenceKind::TypeOf | ReferenceKind::Returns
                ) && reference.name == shadowed
            }),
            "generic parameter or metatype {shadowed} became a type reference: {:?}",
            extracted.references
        );
    }
    let owner = symbol(&extracted, SymbolKind::Field, "owner");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Owner", Some(owner))
    ));
}

#[test]
fn swift_qualified_types_bindings_and_extensions_keep_their_identity() {
    let extracted = extract(
        "Sources/Qualified.swift",
        r"
extension Outer.Inner {
    func extra() {}
}
extension Later {
    func helper() {}
}
struct Bar {}
enum Outer {
    struct Inner {}
    struct Bar {}
}
struct Inner {}
struct Later {
    var a: Foo, b: Baz = make()
    func run(_ value: Outer.Bar, _ text: Swift.String, _ kind: Model.Type) -> Outer.Inner { helper() }
}
",
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "extra").qualified_name,
        "Outer::Inner::extra",
        "a qualified extension reopens the nested type, not the top-level homonym"
    );
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "Inner")
            .count(),
        2,
        "no placeholder is created for a same-file extended type"
    );
    let later = symbol(&extracted, SymbolKind::Struct, "Later");
    let helper = symbol(&extracted, SymbolKind::Method, "helper");
    assert!(
        contains(&extracted, &later.id, &helper.id),
        "an extension declared first still reopens the later type"
    );
    let first = symbol(&extracted, SymbolKind::Field, "a");
    let second = symbol(&extracted, SymbolKind::Field, "b");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Foo", Some(first))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Baz", Some(second))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "make", Some(second))
    ));
    assert!(!has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Baz", Some(first))
    ));
    let run = symbol(&extracted, SymbolKind::Method, "run");
    for (kind, name) in [
        (ReferenceKind::TypeOf, "Outer::Bar"),
        (ReferenceKind::TypeOf, "Model"),
        (ReferenceKind::Returns, "Outer::Inner"),
    ] {
        assert!(
            has_reference(&extracted, ReferenceQuery::new(kind, name, Some(run))),
            "missing {kind:?} {name}: {:?}",
            extracted.references
        );
    }
    for absent in [
        "Bar",
        "Outer",
        "Inner",
        "String",
        "Swift::String",
        "Model::Type",
    ] {
        assert!(
            !has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::TypeOf, absent, Some(run))
            ) && !has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Returns, absent, Some(run))
            ),
            "qualification lost or builtin leaked: {absent}"
        );
    }
}

#[test]
fn swift_extension_scopes_ordering_and_binding_spans() {
    let source = r"
struct T {}
struct Box<T> {}
extension Box {
    func use(_ item: T, _ list: Swift.Array<Model>) {}
}
struct Outer {}
extension Outer.Inner {
    func helper() {}
}
extension Outer {
    struct Inner {
        func run() { helper() }
    }
}
struct Pair {
    let a = 1, b = 2
}
";
    let extracted = extract("Sources/Scopes.swift", source);
    let used = symbol(&extracted, SymbolKind::Method, "use");
    assert!(
        !has_reference(
            &extracted,
            ReferenceQuery::new(ReferenceKind::TypeOf, "T", Some(used))
        ),
        "the reopened type's generic parameter is not the global T"
    );
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Model", Some(used))
    ));
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "Array" && !reference.name.starts_with("Swift")),
        "a Swift-qualified standard-library type is not a project reference"
    );
    let inner = symbols_named(&extracted, SymbolKind::Struct, "Inner");
    assert_eq!(inner.len(), 1);
    assert!(
        symbols_named(&extracted, SymbolKind::Class, "Inner").is_empty(),
        "an extension of an extension-declared type reopens it"
    );
    let helper = symbol(&extracted, SymbolKind::Method, "helper");
    assert!(contains(&extracted, &inner[0].id, &helper.id));

    let first = symbol(&extracted, SymbolKind::Field, "a");
    let second = symbol(&extracted, SymbolKind::Field, "b");
    assert_eq!(first.span, second.span, "every binding spans its statement");
    let changed = extract(
        "Sources/Scopes.swift",
        &source.replace("let a = 1", "let a = 3"),
    );
    assert_ne!(
        symbol(&changed, SymbolKind::Field, "a").structural_digest,
        first.structural_digest,
        "an initializer edit changes its binding's digest"
    );
}

#[test]
fn swift_type_aliases_accessor_locals_and_modifier_combinations() {
    let extracted = extract(
        "Sources/Aliases.swift",
        r"
public typealias Handler = (Int) -> Void
protocol Store {
    associatedtype Entity
}
struct Box {
    typealias Inner = [Item]
    public private(set) var value: Int
    open class var shared: Box { Box() }
    var total: Int {
        let scratch = Helper()
        return scratch.count
    }
}
",
    );
    let handler = symbol(&extracted, SymbolKind::TypeAlias, "Handler");
    assert!(handler.export.exported);
    assert_eq!(
        symbol(&extracted, SymbolKind::TypeAlias, "Entity").qualified_name,
        "Store::Entity"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::TypeAlias, "Inner").qualified_name,
        "Box::Inner"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "value").visibility,
        Some(Visibility::Public)
    );
    let shared = symbol(&extracted, SymbolKind::Property, "shared");
    assert!(shared.execution.static_member);
    assert_eq!(shared.visibility, Some(Visibility::Public));
    let total = symbol(&extracted, SymbolKind::Property, "total");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.name != "scratch"),
        "accessor locals are not declarations"
    );
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "Helper", Some(total))
    ));
}

/// Half-typed declarations whose name tree-sitter recovers as a zero-width
/// MISSING identifier (or does not recover at all).
const NAMELESS_SWIFT_DECLARATIONS: [&str; 6] = [
    "func () {}\nfunc after() {}\n",
    "struct S {\n  var : Int\n  func kept() {}\n}\n",
    "enum E {\n  case\n}\n",
    "typealias = Int\n",
    "struct {}\n",
    "protocol {}\nclass {}\n",
];

#[test]
fn swift_recovered_declarations_without_names_emit_no_symbols() {
    for source in NAMELESS_SWIFT_DECLARATIONS {
        let extracted = extract("Sources/Broken.swift", source);
        for symbol in &extracted.symbols {
            assert!(
                !symbol.name.trim().is_empty()
                    && !symbol.qualified_name.starts_with("::")
                    && !symbol.qualified_name.ends_with("::"),
                "nameless symbol from {source:?}: {symbol:?}"
            );
        }
    }
    symbol(
        &extract("Sources/Broken.swift", NAMELESS_SWIFT_DECLARATIONS[0]),
        SymbolKind::Function,
        "after",
    );
    let members = extract("Sources/Broken.swift", NAMELESS_SWIFT_DECLARATIONS[1]);
    assert_eq!(
        symbol(&members, SymbolKind::Method, "kept").qualified_name,
        "S::kept"
    );
}

#[test]
fn swift_backtick_escaped_declarations_are_named_like_their_uses() {
    let extracted = extract(
        "Sources/Keywords.swift",
        r"
struct `Type` {
    var `default`: Int
    func `repeat`() {}
}
enum Mode { case `self`, plain }
typealias `Protocol` = Int
func `repeat`() {}
func run(_ value: `Type`) { `repeat`(); value.`repeat`() }
extension `Type` { func later() {} }
",
    );
    for symbol in &extracted.symbols {
        assert!(!symbol.name.contains('`'), "escaped name kept: {symbol:?}");
        assert!(
            !symbol.qualified_name.contains('`'),
            "escaped qualified name kept: {symbol:?}"
        );
    }
    let escaped_type = symbol(&extracted, SymbolKind::Struct, "Type");
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "default").qualified_name,
        "Type::default"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "repeat").qualified_name,
        "Type::repeat"
    );
    let later = symbol(&extracted, SymbolKind::Method, "later");
    assert!(
        contains(&extracted, &escaped_type.id, &later.id),
        "an extension of an escaped name reopens the same type"
    );
    symbol(&extracted, SymbolKind::EnumMember, "self");
    symbol(&extracted, SymbolKind::TypeAlias, "Protocol");
    let run = symbol(&extracted, SymbolKind::Function, "run");
    symbol(&extracted, SymbolKind::Function, "repeat");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "repeat", Some(run))
    ));
    assert!(
        has_reference(
            &extracted,
            ReferenceQuery::new(ReferenceKind::Calls, "value.repeat", Some(run))
        ),
        "a qualified call of an escaped member keeps its call: {:?}",
        extracted.references
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.contains('`')),
        "{:?}",
        extracted.references
    );
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::TypeOf, "Type", Some(run))
    ));
}

#[test]
fn swift_attributed_returns_and_nested_extension_generics_name_real_types() {
    let extracted = extract(
        "Sources/Nested.swift",
        r"
func make() -> @Sendable (Model) -> Result { fatalError() }
struct Outer<T> {
    struct Inner<U> {}
}
extension Outer.Inner {
    func pair(_ first: T, _ second: U) -> Payload { fatalError() }
}
",
    );
    let make = symbol(&extracted, SymbolKind::Function, "make");
    for name in ["Model", "Result"] {
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Returns, name, Some(make))
            ),
            "missing returned {name}: {:?}",
            extracted.references
        );
    }
    let pair = symbol(&extracted, SymbolKind::Method, "pair");
    assert_eq!(pair.qualified_name, "Outer::Inner::pair");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Returns, "Payload", Some(pair))
    ));
    for attribute_or_parameter in ["Sendable", "T", "U"] {
        assert!(
            extracted
                .references
                .iter()
                .all(|reference| reference.name != attribute_or_parameter),
            "{attribute_or_parameter} is not a project type: {:?}",
            extracted.references
        );
    }
}

#[test]
fn swift_destructuring_bindings_keep_their_initializer_references() {
    let extracted = extract(
        "Sources/Pairs.swift",
        r"
let (first, second) = (makeFirst(), makeSecond())
struct Pair {
    let (left, right) = (makeLeft(), makeRight())
    let named = makeNamed()
}
",
    );
    for callee in [
        "makeFirst",
        "makeSecond",
        "makeLeft",
        "makeRight",
        "makeNamed",
    ] {
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Calls, callee, None)
            ),
            "missing call {callee}: {:?}",
            extracted.references
        );
    }
    let pair = symbol(&extracted, SymbolKind::Struct, "Pair");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "makeLeft", Some(pair))
    ));
    let named = symbol(&extracted, SymbolKind::Field, "named");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "makeNamed", Some(named))
    ));
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !symbol.name.starts_with('(')),
        "a tuple pattern is not a name"
    );
}

#[test]
fn swift_extraction_is_deterministic_and_swiftui_views_are_structs() {
    assert_eq!(
        extract("Sources/Members.swift", MEMBERS_SAMPLE),
        extract("Sources/Members.swift", MEMBERS_SAMPLE)
    );
    let view = extract(
        "Sources/App.swift",
        "import SwiftUI\nstruct ContentView: View { var body: some View { Text(\"Hi\") } }\n",
    );
    symbol(&view, SymbolKind::Struct, "ContentView");
    symbol(&view, SymbolKind::Component, "ContentView");
}

#[test]
fn swift_optional_chained_and_force_unwrapped_calls_keep_their_call() {
    let extracted = extract(
        "Sources/Cells/Profile.swift",
        "class Profile {\n    func load() {\n        delegate?.profileDidUpdate(user)\n        cache!.flush()\n        a?.b?.c()\n        let ready = flag ? check() : skip()\n        \"swift_literal_sentinel?.member\"()\n        \"swift_tick`sentinel\"()\n        \"swift_plain_sentinel\"()\n    }\n}\n",
    );
    let load = symbol(&extracted, SymbolKind::Method, "load");
    for name in [
        "delegate.profileDidUpdate",
        "cache.flush",
        "a.b.c",
        "check",
        "skip",
    ] {
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Calls, name, Some(load))
            ),
            "missing call {name}: {:?}",
            extracted.references
        );
    }
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.contains(['?', '!'])
                && !reference.name.contains("sentinel")),
        "unwrap markers and string contents never enter a name: {:?}",
        extracted.references
    );
}

#[test]
fn swiftui_main_app_entry_is_a_class_landmark() {
    // v1 resolution/frameworks/swift.ts marks `@main struct X: App` as the
    // SwiftUI app entry, a class node beside the struct declaration.
    let extracted = extract(
        "Sources/App/ShopApp.swift",
        "import SwiftUI\n\n@main\nstruct ShopApp: App {\n    var body: some Scene { WindowGroup { ContentView() } }\n}\n\nstruct Helper: App {}\n",
    );
    let entry = symbol(&extracted, SymbolKind::Class, "ShopApp");
    assert_eq!(entry.span.start_line(), 3);
    assert!(
        entry.implementation.declaration_only,
        "the struct, not the landmark, defines the type"
    );
    let shop = symbol(&extracted, SymbolKind::Struct, "ShopApp");
    assert!(!shop.implementation.declaration_only);
    assert!(
        symbols_named(&extracted, SymbolKind::Class, "Helper").is_empty(),
        "only the @main entry is an app landmark"
    );

    let commented = extract(
        "Sources/App/Entries.swift",
        "import SwiftUI\n@main /* { */ struct Shop: App { var body: some Scene { WindowGroup {} } }\n",
    );
    symbol(&commented, SymbolKind::Class, "Shop");
    let impostors = extract(
        "Sources/App/Cli.swift",
        "import SwiftUI\n@main struct Cli: Entry /* App */ { static func main() {} }\n",
    );
    assert!(
        symbols_named(&impostors, SymbolKind::Class, "Cli").is_empty(),
        "a commented-out base is no conformance"
    );
    let constrained = extract(
        "Sources/App/Box.swift",
        "import SwiftUI\n@main struct Box<T>: Entry where T: App { static func main() {} }\n",
    );
    assert!(
        symbols_named(&constrained, SymbolKind::Class, "Box").is_empty(),
        "a generic constraint is no conformance"
    );

    let attributed = extract(
        "Sources/App/Legacy.swift",
        "import SwiftUI\n@available(*, deprecated, message: \"a struct { kept }\")\n@main struct Legacy: App { var body: some Scene { WindowGroup {} } }\n",
    );
    symbol(&attributed, SymbolKind::Class, "Legacy");
    for (path, source, name) in [
        (
            "Sources/App/Gen.swift",
            "import SwiftUI\n@main struct Gen<T: App>: Entry { static func main() {} }\n",
            "Gen",
        ),
        (
            "Sources/App/Wrap.swift",
            "import SwiftUI\n@main struct Wrap: Entry<App> { static func main() {} }\n",
            "Wrap",
        ),
    ] {
        assert!(
            symbols_named(&extract(path, source), SymbolKind::Class, name).is_empty(),
            "{name}: an `App` generic argument or parameter bound is no conformance"
        );
    }
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind:?} {name}: {extracted:?}"))
}

fn symbols_named<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> Vec<&'file ExtractedSymbol> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind && symbol.name == name)
        .collect()
}

fn contains(
    extracted: &ExtractedFile,
    parent: &cartograph_domain::SymbolId,
    child: &cartograph_domain::SymbolId,
) -> bool {
    extracted
        .containments
        .iter()
        .any(|containment| &containment.parent == parent && &containment.child == child)
}

/// The kind, name, and optional owner a reference must have.
#[derive(Clone, Copy)]
struct ReferenceQuery<'query> {
    kind: ReferenceKind,
    name: &'query str,
    owner: Option<&'query ExtractedSymbol>,
}

impl<'query> ReferenceQuery<'query> {
    const fn new(
        kind: ReferenceKind,
        name: &'query str,
        owner: Option<&'query ExtractedSymbol>,
    ) -> Self {
        Self { kind, name, owner }
    }
}

fn has_reference(extracted: &ExtractedFile, query: ReferenceQuery<'_>) -> bool {
    extracted.references.iter().any(|reference| {
        reference.kind == query.kind
            && reference.name == query.name
            && query
                .owner
                .is_none_or(|owner| reference.owner.as_ref() == Some(&owner.id))
    })
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::Swift, "{path}");
    let mut extractor = NativeExtractor::new(SourceLanguage::Swift)
        .unwrap_or_else(|error| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}
