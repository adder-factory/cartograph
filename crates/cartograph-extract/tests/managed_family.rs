//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, ImportBindingKind, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

const JAVA_ORACLE: &str = include_str!("fixtures/v1_1_33/OrderService.java");
const CSHARP_ORACLE: &str = include_str!("fixtures/v1_1_33/OrderService.cs");

#[test]
fn java_preserves_the_v1_oracle_and_adds_package_and_reference_semantics() {
    let extracted = extract(
        "src/main/java/com/acme/orders/OrderService.java",
        JAVA_ORACLE,
    );
    assert_eq!(extracted.language, SourceLanguage::Java);

    let package = symbol(
        &extracted,
        SymbolKind::Namespace,
        "com.acme.orders",
        "com.acme.orders",
    );
    let service = symbol(
        &extracted,
        SymbolKind::Class,
        "OrderService",
        "com.acme.orders::OrderService",
    );
    assert_eq!(service.visibility, Some(Visibility::Public));
    assert!(service.export.exported);
    assert_containment(&extracted, package, service);
    assert_reference_owned_by(&extracted, service, "Deprecated", ReferenceKind::Decorates);
    assert_reference_owned_by(&extracted, service, "BaseService", ReferenceKind::Extends);
    assert_reference_owned_by(&extracted, service, "Closeable", ReferenceKind::Implements);
    assert_reference_owned_by(&extracted, service, "Auditable", ReferenceKind::Implements);

    let repository = symbol(
        &extracted,
        SymbolKind::Field,
        "repository",
        "com.acme.orders::OrderService::repository",
    );
    assert_eq!(
        repository.signature.as_deref(),
        Some("Repository repository")
    );
    assert_eq!(repository.visibility, Some(Visibility::Private));
    assert!(!repository.execution.static_member);
    assert_reference_owned_by(&extracted, repository, "Repository", ReferenceKind::TypeOf);

    let kind = symbol(
        &extracted,
        SymbolKind::Field,
        "KIND",
        "com.acme.orders::OrderService::KIND",
    );
    assert_eq!(kind.signature.as_deref(), Some("String KIND"));
    assert!(kind.execution.static_member);

    let constructor = symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "com.acme.orders::OrderService::OrderService",
        "(Repository repository)",
    );
    assert_eq!(
        constructor.docstring.as_deref(),
        Some("Creates the service.")
    );
    assert_reference_owned_by(&extracted, constructor, "Repository", ReferenceKind::TypeOf);

    let find = symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "com.acme.orders::OrderService::find",
        "List<Order> (String id)",
    );
    assert_reference_owned_by(&extracted, find, "Override", ReferenceKind::Decorates);
    assert_reference_owned_by(&extracted, find, "List", ReferenceKind::Returns);
    assert_reference_owned_by(&extracted, find, "Order", ReferenceKind::Returns);
    assert_reference_owned_by(&extracted, find, "String", ReferenceKind::TypeOf);
    assert_reference_owned_by(&extracted, find, "Order", ReferenceKind::Instantiates);
    assert_reference_owned_by(
        &extracted,
        find,
        "repository.findById",
        ReferenceKind::Calls,
    );
    assert_eq!(
        extracted
            .references
            .iter()
            .filter(|reference| {
                reference.owner.as_ref() == Some(&find.id)
                    && reference.name == "repository.findById"
                    && reference.kind == ReferenceKind::Calls
            })
            .count(),
        2,
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "this.repository.findById")
    );

    let add = symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "com.acme.orders::OrderService::add",
        "int (int left, int right)",
    );
    assert!(add.execution.static_member);
    assert_eq!(add.visibility, Some(Visibility::Public));
    assert!(!format!("{extracted:?}").contains("sk_live_java_secret"));
}

#[test]
fn java_extracts_interfaces_records_enums_annotations_and_overloads() {
    let extracted = extract(
        "src/main/java/com/acme/types/Types.java",
        r#"package com.acme.types;

public interface Handler extends AutoCloseable {
    String VERSION = "sk_live_interface_secret";
    Result handle(Input input);
}

public record Result(String value, int code) implements java.io.Serializable {}

public enum Status {
    READY,
    FAILED
}

public @interface Route {
    String value();
}

public class Overloads {
    public void run(String value) {}
    public void run(int value) {}
}
"#,
    );

    let handler = symbol(
        &extracted,
        SymbolKind::Interface,
        "Handler",
        "com.acme.types::Handler",
    );
    assert_reference_owned_by(&extracted, handler, "AutoCloseable", ReferenceKind::Extends);
    let handle = symbol(
        &extracted,
        SymbolKind::Method,
        "handle",
        "com.acme.types::Handler::handle",
    );
    assert!(handle.implementation.declaration_only);
    assert_eq!(handle.visibility, Some(Visibility::Public));
    let version = symbol(
        &extracted,
        SymbolKind::Field,
        "VERSION",
        "com.acme.types::Handler::VERSION",
    );
    assert!(version.execution.static_member);
    assert_eq!(version.visibility, Some(Visibility::Public));
    assert_eq!(version.signature.as_deref(), Some("String VERSION"));
    assert!(!format!("{extracted:?}").contains("sk_live_interface_secret"));

    let record = symbol(
        &extracted,
        SymbolKind::Struct,
        "Result",
        "com.acme.types::Result",
    );
    assert_reference_owned_by(
        &extracted,
        record,
        "java.io.Serializable",
        ReferenceKind::Implements,
    );
    symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "com.acme.types::Result::Result",
        "(String value, int code)",
    );

    let status = symbol(
        &extracted,
        SymbolKind::Enum,
        "Status",
        "com.acme.types::Status",
    );
    let ready = symbol(
        &extracted,
        SymbolKind::EnumMember,
        "READY",
        "com.acme.types::Status::READY",
    );
    assert_containment(&extracted, status, ready);
    symbol(
        &extracted,
        SymbolKind::Interface,
        "Route",
        "com.acme.types::Route",
    );

    let overloads = extracted
        .symbols
        .iter()
        .filter(|candidate| candidate.qualified_name == "com.acme.types::Overloads::run")
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2, "facts={:?}", symbol_facts(&extracted));
    assert_ne!(overloads[0].id, overloads[1].id);
    assert_eq!(
        overloads
            .iter()
            .filter_map(|candidate| candidate.signature.as_deref())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["void (String value)", "void (int value)"]),
    );
}

#[test]
fn csharp_preserves_the_v1_oracle_and_adds_namespace_and_reference_semantics() {
    let extracted = extract("src/Acme.Orders/OrderService.cs", CSHARP_ORACLE);
    assert_eq!(extracted.language, SourceLanguage::CSharp);

    let namespace = symbol(
        &extracted,
        SymbolKind::Namespace,
        "Acme.Orders",
        "Acme.Orders",
    );
    let service = symbol(
        &extracted,
        SymbolKind::Class,
        "OrderService",
        "Acme.Orders::OrderService",
    );
    assert_containment(&extracted, namespace, service);
    assert_eq!(service.visibility, Some(Visibility::Public));
    assert_reference_owned_by(
        &extracted,
        service,
        "Serializable",
        ReferenceKind::Decorates,
    );
    assert_reference_owned_by(&extracted, service, "BaseService", ReferenceKind::Inherits);
    assert_reference_owned_by(&extracted, service, "IDisposable", ReferenceKind::Inherits);

    let primary = symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "Acme.Orders::OrderService::OrderService",
        "(IRepository repository)",
    );
    assert_reference_owned_by(&extracted, primary, "IRepository", ReferenceKind::TypeOf);

    let repository = symbol(
        &extracted,
        SymbolKind::Field,
        "_repository",
        "Acme.Orders::OrderService::_repository",
    );
    assert_eq!(
        repository.signature.as_deref(),
        Some("IRepository _repository")
    );
    assert_eq!(repository.visibility, Some(Visibility::Private));

    let name = symbol(
        &extracted,
        SymbolKind::Property,
        "Name",
        "Acme.Orders::OrderService::Name",
    );
    assert_eq!(name.signature.as_deref(), Some("string Name"));
    assert_eq!(name.visibility, Some(Visibility::Public));

    let get = symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "Acme.Orders::OrderService::GetOrderAsync",
        "Task<Order> (string id)",
    );
    assert!(get.execution.async_symbol);
    assert_eq!(
        get.docstring.as_deref(),
        Some("<summary>Gets an order.</summary>")
    );
    assert_reference_owned_by(&extracted, get, "Obsolete", ReferenceKind::Decorates);
    assert_reference_owned_by(&extracted, get, "Task", ReferenceKind::Returns);
    assert_reference_owned_by(&extracted, get, "Order", ReferenceKind::Returns);
    assert_reference_owned_by(&extracted, get, "Order", ReferenceKind::Instantiates);
    assert_reference_owned_by(
        &extracted,
        get,
        "this._repository.FindById",
        ReferenceKind::Calls,
    );
    assert_reference_owned_by(
        &extracted,
        get,
        "ConsoleAlias.WriteLine",
        ReferenceKind::Calls,
    );
    assert!(!format!("{extracted:?}").contains("sk_live_csharp_secret"));
    assert!(!format!("{extracted:?}").contains("sk_live_attribute_secret"));
}

#[test]
fn csharp_extracts_interfaces_records_structs_enums_and_overloads() {
    let extracted = extract(
        "src/Types.cs",
        r"namespace Acme.Types
{
    public interface IHandler : IDisposable
    {
        Result Handle(Input input);
    }

    public readonly record struct Result(string Value, int Code);
    internal struct Point { public int X; public int Y; }
    public record Customer(string Name) : Entity, IAuditable;
    public enum Status { Ready = 1, Failed = 2 }

    public class Overloads
    {
        public void Run(string value) {}
        public void Run(int value) {}
    }
}
",
    );

    let handler = symbol(
        &extracted,
        SymbolKind::Interface,
        "IHandler",
        "Acme.Types::IHandler",
    );
    assert_reference_owned_by(&extracted, handler, "IDisposable", ReferenceKind::Extends);
    assert!(
        symbol(
            &extracted,
            SymbolKind::Method,
            "Handle",
            "Acme.Types::IHandler::Handle",
        )
        .implementation
        .declaration_only
    );

    let result = symbol(
        &extracted,
        SymbolKind::Struct,
        "Result",
        "Acme.Types::Result",
    );
    assert_eq!(result.visibility, Some(Visibility::Public));
    symbol_with_signature(
        &extracted,
        SymbolKind::Method,
        "Acme.Types::Result::Result",
        "(string Value, int Code)",
    );

    let point = symbol(&extracted, SymbolKind::Struct, "Point", "Acme.Types::Point");
    assert_eq!(point.visibility, Some(Visibility::Internal));

    let customer = symbol(
        &extracted,
        SymbolKind::Struct,
        "Customer",
        "Acme.Types::Customer",
    );
    assert_reference_owned_by(&extracted, customer, "Entity", ReferenceKind::Inherits);
    assert_reference_owned_by(&extracted, customer, "IAuditable", ReferenceKind::Inherits);

    symbol(
        &extracted,
        SymbolKind::EnumMember,
        "Ready",
        "Acme.Types::Status::Ready",
    );
    let overloads = extracted
        .symbols
        .iter()
        .filter(|candidate| candidate.qualified_name == "Acme.Types::Overloads::Run")
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2);
    assert_ne!(overloads[0].id, overloads[1].id);
}

#[test]
fn managed_imports_emit_literal_safe_symbols_references_and_exact_bindings() {
    let java = extract(
        "src/Imports.java",
        "import java.util.List;\nimport static java.util.Collections.emptyList;\nimport java.time.*;\n",
    );
    for name in [
        "java.util.List",
        "java.util.Collections.emptyList",
        "java.time",
    ] {
        symbol(&java, SymbolKind::Import, name, name);
        assert_file_reference(&java, name, ReferenceKind::Imports);
    }
    assert_binding(&java, ImportBindingKind::Named, "java.util", "List", "List");
    assert_binding(
        &java,
        ImportBindingKind::Named,
        "java.util.Collections",
        "emptyList",
        "emptyList",
    );
    assert_binding(&java, ImportBindingKind::Namespace, "java.time", "*", "*");

    let csharp = extract(
        "src/Imports.cs",
        "using System;\nusing Alias = System.Collections.Generic.List<int>;\nusing static System.Math;\n",
    );
    for name in [
        "System",
        "System.Collections.Generic.List<int>",
        "System.Math",
    ] {
        symbol(&csharp, SymbolKind::Import, name, name);
        assert_file_reference(&csharp, name, ReferenceKind::Imports);
    }
    assert_binding(
        &csharp,
        ImportBindingKind::Namespace,
        "System.Collections.Generic.List<int>",
        "*",
        "Alias",
    );
    assert_binding(
        &csharp,
        ImportBindingKind::Namespace,
        "System.Math",
        "*",
        "*",
    );
}

#[test]
fn managed_import_keywords_require_token_boundaries() {
    let java = extract(
        "src/StaticPrefix.java",
        "import staticfactory.Client;\nclass StaticPrefix {}\n",
    );
    symbol(
        &java,
        SymbolKind::Import,
        "staticfactory.Client",
        "staticfactory.Client",
    );
    assert_binding(
        &java,
        ImportBindingKind::Named,
        "staticfactory",
        "Client",
        "Client",
    );

    let csharp = extract(
        "src/StaticPrefix.cs",
        "using staticFactory.Tools;\nclass StaticPrefix {}\n",
    );
    symbol(
        &csharp,
        SymbolKind::Import,
        "staticFactory.Tools",
        "staticFactory.Tools",
    );
    assert!(!csharp.import_bindings.iter().any(|binding| {
        binding.module_specifier == "Factory.Tools" || binding.local_name == "*"
    }));
}

#[test]
fn csharp_ambiguous_class_bases_remain_typed_for_resolution() {
    let extracted = extract(
        "src/Bases.cs",
        r"class IPhone {}
interface Disposable {}
class Device : IPhone, Disposable {}
class MissingDevice : MissingBase {}
",
    );
    let device = symbol(&extracted, SymbolKind::Class, "Device", "Device");
    assert_reference_owned_by(&extracted, device, "IPhone", ReferenceKind::Inherits);
    assert_reference_owned_by(&extracted, device, "Disposable", ReferenceKind::Inherits);
    let missing = symbol(
        &extracted,
        SymbolKind::Class,
        "MissingDevice",
        "MissingDevice",
    );
    assert_reference_owned_by(&extracted, missing, "MissingBase", ReferenceKind::Inherits);
}

#[test]
fn managed_attributes_and_default_values_never_copy_literal_credentials() {
    let csharp = extract(
        "src/Safe.cs",
        r#"namespace Safe;
public class Client
{
    private string token = "sk_live_field_secret";

    [Secret("sk_live_attribute_secret")]
    public Task Connect(string value = "sk_live_default_secret", int retries = 3)
    {
        Logger.Write(value);
        return Task.CompletedTask;
    }
}
"#,
    );
    let connect = symbol(
        &csharp,
        SymbolKind::Method,
        "Connect",
        "Safe::Client::Connect",
    );
    assert!(connect.signature.is_none());
    assert_reference_owned_by(&csharp, connect, "Secret", ReferenceKind::Decorates);
    let rendered = format!("{csharp:?}");
    for credential in [
        "sk_live_field_secret",
        "sk_live_attribute_secret",
        "sk_live_default_secret",
    ] {
        assert!(
            !rendered.contains(credential),
            "credential leaked: {credential}"
        );
    }
}

#[test]
fn managed_modes_are_production_admitted_through_the_reviewed_family() {
    let limits = source_limits();
    let java = SourceSnapshot::from_bytes("src/Main.java", b"class Main {}\n", limits)
        .unwrap_or_else(|error| panic!("Java production snapshot failed: {error}"));
    let csharp = SourceSnapshot::from_bytes("src/Main.cs", b"class Main {}\n", limits)
        .unwrap_or_else(|error| panic!("C# production snapshot failed: {error}"));
    assert_eq!(java.language(), SourceLanguage::Java);
    assert_eq!(csharp.language(), SourceLanguage::CSharp);
    assert!(NativeExtractor::new(SourceLanguage::Java).is_ok());
    assert!(NativeExtractor::new(SourceLanguage::CSharp).is_ok());
}

#[test]
fn managed_syntax_damage_is_recoverable_and_diagnostic() {
    for (path, source) in [
        ("src/Broken.java", "public class Broken { void run( {\n"),
        ("src/Broken.cs", "public class Broken { void Run( {\n"),
    ] {
        let extracted = extract(path, source);
        assert_eq!(extracted.parse_status, FileParseStatus::Partial, "{path}");
        assert!(!extracted.diagnostics.is_empty(), "{path}");
    }
}

#[test]
fn managed_cancellation_nesting_and_output_bounds_are_explicit() {
    for (path, source, language) in [
        ("src/Cancel.java", "class Cancel {}\n", SourceLanguage::Java),
        ("src/Cancel.cs", "class Cancel {}\n", SourceLanguage::CSharp),
    ] {
        let snapshot = capability_snapshot(path, source);
        let mut extractor = capability_extractor(language);
        assert_eq!(
            extractor.extract_with_cancellation(&snapshot, || true),
            Err(ExtractError::Cancelled),
            "{path}",
        );
    }
    let mut decorated = String::new();
    for index in 0..2_000 {
        decorated.push_str("[Marker");
        decorated.push_str(&index.to_string());
        decorated.push_str("]\n");
    }
    decorated.push_str("public class Decorated {}\n");
    let snapshot = capability_snapshot("src/Decorated.cs", &decorated);
    let mut extractor = capability_extractor(SourceLanguage::CSharp);
    let mut probes = 0_usize;
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || {
            probes = probes.saturating_add(1);
            probes > 512
        }),
        Err(ExtractError::Cancelled),
    );
    assert!(probes > 512, "cancellation did not occur after progress");

    let mut nested = String::new();
    for index in 0..300 {
        nested.push_str("namespace N");
        nested.push_str(&index.to_string());
        nested.push_str(" {\n");
    }
    nested.push_str("class Deep {}\n");
    nested.push_str(&"}\n".repeat(300));
    let recovered = extract_result("src/Deep.cs", &nested)
        .unwrap_or_else(|error| panic!("deep C# extraction did not recover: {error}"));
    assert_eq!(
        recovered.parse_status,
        cartograph_domain::FileParseStatus::Partial
    );
    assert_eq!(
        recovered.diagnostics[0].code,
        cartograph_extract::DiagnosticCode::NestingLimitExceeded
    );

    let mut excessive = String::from("class Many {\n");
    for index in 0..20_000 {
        excessive.push_str("int value_");
        excessive.push_str(&index.to_string());
        excessive.push_str(";\n");
    }
    excessive.push_str("}\n");
    assert_eq!(
        extract_result("src/Many.java", &excessive),
        Err(ExtractError::OutputLimit),
    );
}

#[test]
fn managed_facts_are_repeatable_and_locked() {
    let cases = [
        (
            "src/main/java/com/acme/orders/OrderService.java",
            JAVA_ORACLE,
            "25ac822c4d0ee6841e5692eb38c67b4eeb204e8677814519522180252b3ba450",
        ),
        (
            "src/Acme.Orders/OrderService.cs",
            CSHARP_ORACLE,
            "8f228bf5a46aa39af0d950cc80dc6a1226921c472a5288ca1aafc8a080f03674",
        ),
    ];
    for (path, source, expected) in cases {
        let first = extract(path, source);
        let second = extract(path, source);
        assert_eq!(canonical_facts(&first), canonical_facts(&second), "{path}");
        assert_unique_ids(&first);
        assert_eq!(
            locked_digest(&first),
            expected,
            "{path}; facts={:?}",
            canonical_facts(&first),
        );
    }
}

#[test]
fn java_heritage_names_only_the_generic_head_type() {
    let extracted = extract(
        "src/Foo.java",
        "class Foo extends Bar<Baz> implements Map<String, Qux>, pkg.Pair<Left, Right> {}\n\
         class Nested extends Outer<Arg>.Inner<Other> {}\n\
         interface J extends K<U> {}\n\
         class G { List<Item> load(Map<String, Row> rows) { return null; } }\n",
    );
    let foo = symbol(&extracted, SymbolKind::Class, "Foo", "Foo");
    assert_reference_owned_by(&extracted, foo, "Bar", ReferenceKind::Extends);
    assert_reference_owned_by(&extracted, foo, "Map", ReferenceKind::Implements);
    assert_reference_owned_by(&extracted, foo, "pkg.Pair", ReferenceKind::Implements);
    let interface = symbol(&extracted, SymbolKind::Interface, "J", "J");
    assert_reference_owned_by(&extracted, interface, "K", ReferenceKind::Extends);
    let nested = symbol(&extracted, SymbolKind::Class, "Nested", "Nested");
    assert_reference_owned_by(&extracted, nested, "Outer.Inner", ReferenceKind::Extends);
    for argument in [
        "Baz", "String", "Qux", "Left", "Right", "U", "Arg", "Other", "Outer",
    ] {
        assert!(
            extracted.references.iter().all(|reference| {
                reference.name != argument
                    || !matches!(
                        reference.kind,
                        ReferenceKind::Extends | ReferenceKind::Implements
                    )
            }),
            "generic argument {argument} became an inheritance target: {:?}",
            reference_facts(&extracted),
        );
    }
    // Type usage (not inheritance) still names its generic arguments.
    let load = symbol(&extracted, SymbolKind::Method, "load", "G::load");
    assert_reference_owned_by(&extracted, load, "Item", ReferenceKind::Returns);
    assert_reference_owned_by(&extracted, load, "Row", ReferenceKind::TypeOf);
}

#[test]
fn csharp_tuple_types_name_element_types_but_never_element_names() {
    let extracted = extract(
        "src/K.cs",
        "class K { void Load((Item first, int count) pair) {} (Row row, Item item) Pair() { return default; } }\n",
    );
    let load = symbol(&extracted, SymbolKind::Method, "Load", "K::Load");
    assert_reference_owned_by(&extracted, load, "Item", ReferenceKind::TypeOf);
    let pair = symbol(&extracted, SymbolKind::Method, "Pair", "K::Pair");
    assert_reference_owned_by(&extracted, pair, "Row", ReferenceKind::Returns);
    assert_reference_owned_by(&extracted, pair, "Item", ReferenceKind::Returns);
    for element in ["first", "count", "row", "item", "int"] {
        assert!(
            extracted
                .references
                .iter()
                .all(|reference| reference.name != element),
            "tuple element {element} became a type reference: {:?}",
            reference_facts(&extracted),
        );
    }
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    extract_result(path, source)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}

fn extract_result(path: &str, source: &str) -> Result<ExtractedFile, ExtractError> {
    let snapshot = capability_snapshot(path, source);
    capability_extractor(snapshot.language()).extract(&snapshot)
}

fn capability_snapshot(path: &str, source: &str) -> SourceSnapshot {
    SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), source_limits())
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"))
}

fn source_limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT).unwrap_or_else(|error| panic!("source limits failed: {error}"))
}

fn capability_extractor(language: SourceLanguage) -> NativeExtractor {
    NativeExtractor::new_for_capability_validation(language)
        .unwrap_or_else(|error| panic!("{language:?} capability extractor failed: {error}"))
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
    qualified_name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|candidate| {
            candidate.kind == kind
                && candidate.name == name
                && candidate.qualified_name == qualified_name
        })
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name}; facts={:?}",
                symbol_facts(extracted),
            )
        })
}

fn symbol_with_signature<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
    signature: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|candidate| {
            candidate.kind == kind
                && candidate.qualified_name == qualified_name
                && candidate.signature.as_deref() == Some(signature)
        })
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name} with {signature}; facts={:?}",
                symbol_facts(extracted),
            )
        })
}

fn assert_containment(
    extracted: &ExtractedFile,
    parent: &ExtractedSymbol,
    child: &ExtractedSymbol,
) {
    assert!(
        extracted
            .containments
            .iter()
            .any(|edge| edge.parent == parent.id && edge.child == child.id),
        "missing containment {} -> {}",
        parent.qualified_name,
        child.qualified_name,
    );
}

fn assert_reference_owned_by(
    extracted: &ExtractedFile,
    owner: &ExtractedSymbol,
    name: &str,
    kind: ReferenceKind,
) {
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&owner.id)
                && reference.name == name
                && reference.kind == kind
        }),
        "missing {kind:?} {name} owned by {}; references={:?}",
        owner.qualified_name,
        reference_facts(extracted),
    );
}

fn assert_file_reference(extracted: &ExtractedFile, name: &str, kind: ReferenceKind) {
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.is_none() && reference.name == name && reference.kind == kind
        }),
        "missing file-level {kind:?} {name}; references={:?}",
        reference_facts(extracted),
    );
}

fn assert_binding(
    extracted: &ExtractedFile,
    kind: ImportBindingKind,
    module: &str,
    imported: &str,
    local: &str,
) {
    assert!(
        extracted.import_bindings.iter().any(|binding| {
            binding.kind == kind
                && binding.module_specifier == module
                && binding.imported_name == imported
                && binding.local_name == local
        }),
        "missing {kind:?} binding {module}|{imported}|{local}; bindings={:?}",
        extracted.import_bindings,
    );
}

fn assert_unique_ids(extracted: &ExtractedFile) {
    let ids = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), extracted.symbols.len());
}

fn symbol_names(extracted: &ExtractedFile) -> BTreeMap<&str, &str> {
    extracted
        .symbols
        .iter()
        .map(|symbol| (symbol.id.as_str(), symbol.qualified_name.as_str()))
        .collect()
}

fn symbol_facts(extracted: &ExtractedFile) -> Vec<String> {
    extracted
        .symbols
        .iter()
        .map(|symbol| {
            format!(
                "{}|{}|{}|{:?}|decl={}|export={}|async={}|static={}|sig={:?}",
                symbol.kind.as_str(),
                symbol.name,
                symbol.qualified_name,
                symbol.visibility,
                symbol.implementation.declaration_only,
                symbol.export.exported,
                symbol.execution.async_symbol,
                symbol.execution.static_member,
                symbol.signature,
            )
        })
        .collect()
}

fn reference_facts(extracted: &ExtractedFile) -> Vec<String> {
    let names = symbol_names(extracted);
    extracted
        .references
        .iter()
        .map(|reference| {
            format!(
                "{}|{}|{}",
                reference
                    .owner
                    .as_ref()
                    .and_then(|owner| names.get(owner.as_str()).copied())
                    .unwrap_or("<file>"),
                reference.kind.as_str(),
                reference.name,
            )
        })
        .collect()
}

fn canonical_facts(extracted: &ExtractedFile) -> Vec<String> {
    let names = symbol_names(extracted);
    let mut facts = vec![format!(
        "F|{}|{:?}|{}|{}|{}",
        extracted.language.as_str(),
        extracted.parse_status,
        extracted.symbols.len(),
        extracted.references.len(),
        extracted.import_bindings.len(),
    )];
    facts.extend(extracted.symbols.iter().map(|symbol| {
        format!(
            "S|{}|{}|{}|{}-{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
            symbol.kind.as_str(),
            symbol.name,
            symbol.qualified_name,
            symbol.span.start_byte(),
            symbol.span.end_byte(),
            symbol.structural_digest,
            symbol.signature.as_deref().unwrap_or_default(),
            symbol.docstring.as_deref().unwrap_or_default(),
            symbol.body_search_text,
            symbol.implementation.declaration_only,
            symbol.export.exported,
            symbol.execution.async_symbol,
            symbol.execution.static_member,
            symbol.visibility,
        )
    }));
    facts.extend(extracted.containments.iter().map(|edge| {
        format!(
            "C|{}|{}",
            names
                .get(edge.parent.as_str())
                .copied()
                .unwrap_or("<missing>"),
            names
                .get(edge.child.as_str())
                .copied()
                .unwrap_or("<missing>"),
        )
    }));
    facts.extend(extracted.references.iter().map(|reference| {
        format!(
            "R|{}|{}|{}|{}-{}",
            reference
                .owner
                .as_ref()
                .and_then(|owner| names.get(owner.as_str()).copied())
                .unwrap_or("<file>"),
            reference.kind.as_str(),
            reference.name,
            reference.span.start_byte(),
            reference.span.end_byte(),
        )
    }));
    facts.extend(extracted.import_bindings.iter().map(|binding| {
        format!(
            "I|{:?}|{}|{}|{}|{}-{}",
            binding.kind,
            binding.module_specifier,
            binding.imported_name,
            binding.local_name,
            binding.span.start_byte(),
            binding.span.end_byte(),
        )
    }));
    facts.extend(
        extracted
            .diagnostics
            .iter()
            .map(|diagnostic| format!("D|{:?}|{:?}", diagnostic.code, diagnostic.span)),
    );
    facts
}

fn locked_digest(extracted: &ExtractedFile) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("cartograph.v2.managed-family.2026-07-24");
    for fact in canonical_facts(extracted) {
        hasher.update(&u64::try_from(fact.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(fact.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}
