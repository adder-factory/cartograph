//! VB.NET extraction contracts (v1 `vbnet` extractor parity).

mod dependency_ownership;

use std::{collections::BTreeSet, fmt::Write};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, ImportBindingKind, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
const LITERAL_SENTINEL: &str = "vb_literal_sentinel_4d2a";

#[test]
fn vbnet_wide_recovered_heritage_keeps_distinct_owned_targets() {
    let mut source = String::new();
    for index in 0..192 {
        write!(source, "Class Child{index}\n Inherits Base_{index}\n Implements I_{index}, J_{index}\nEnd Class\n")
            .unwrap_or_else(|error| panic!("test source failed: {error}"));
    }
    let extracted = extract("src/Heritage.vb", &source);
    let heritage = extracted
        .references
        .iter()
        .filter(|reference| {
            matches!(
                reference.kind,
                ReferenceKind::Extends | ReferenceKind::Implements
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(heritage.len(), 192 * 3);
    for index in 0..192 {
        let child = symbol(&extracted, SymbolKind::Class, &format!("Child{index}"));
        for (name, kind) in [
            (format!("Base_{index}"), ReferenceKind::Extends),
            (format!("I_{index}"), ReferenceKind::Implements),
            (format!("J_{index}"), ReferenceKind::Implements),
        ] {
            assert_reference(&extracted, child, (&name, kind));
        }
    }
}

#[test]
fn vbnet_extracts_v1_imports_classes_properties_methods_and_signatures() {
    let extracted = extract(
        "src/Greeter.vb",
        "Imports System\n\nPublic Class Greeter\n  Public Property Name As String\n  Public Sub SayHello()\n    Console.WriteLine(Name)\n  End Sub\n  Public Shared Function Echo(value As String) As String\n    Return value\n  End Function\nEnd Class\n",
    );
    assert!(
        extracted.diagnostics.is_empty(),
        "{:?}",
        extracted.diagnostics
    );
    symbol(&extracted, SymbolKind::Import, "System");
    assert!(extracted.references.iter().any(|reference| {
        reference.kind == ReferenceKind::Imports && reference.name == "System"
    }));
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Namespace && binding.module_specifier == "System"
    }));
    let greeter = symbol(&extracted, SymbolKind::Class, "Greeter");
    assert_eq!(greeter.visibility, Some(Visibility::Public));
    let name = symbol(&extracted, SymbolKind::Property, "Greeter::Name");
    assert_eq!(name.signature.as_deref(), Some("Name As String"));
    let say_hello = symbol(&extracted, SymbolKind::Method, "Greeter::SayHello");
    assert_eq!(say_hello.signature.as_deref(), Some("()"));
    assert!(!say_hello.implementation.declaration_only);
    let echo = symbol(&extracted, SymbolKind::Method, "Greeter::Echo");
    assert_eq!(
        echo.signature.as_deref(),
        Some("(value As String) As String")
    );
    assert!(echo.execution.static_member);
    assert!(!say_hello.execution.static_member);
    assert_reference(
        &extracted,
        say_hello,
        ("Console.WriteLine", ReferenceKind::Calls),
    );
}

#[test]
fn vbnet_types_have_their_own_kinds_members_fields_and_calls() {
    let source = format!(
        "Imports System\n\nPublic Interface IGreeter\n  Function Greet(value As String) As String\nEnd Interface\n\nPublic Structure Pt\n  Public X As Integer\nEnd Structure\n\nPublic Enum Color\n  Red\n  Green\nEnd Enum\n\nPublic Class Person\n  Inherits BasePerson\n  Implements IGreeter, IDisposable\n  Private _name As String\n  Public x As Integer, y As Integer\n  Friend Shared Function Greet(value As String) As String Implements IGreeter.Greet\n    Dim n As Integer = 1\n    Dim label As String = \"{LITERAL_SENTINEL}\"\n    Helper(n)\n    Console.WriteLine(_name)\n    Return New Formatter().Format(value)\n  End Function\n  Sub F()\n    Me.Helper()\n  End Sub\n  Public Sub New(id As Integer)\n  End Sub\nEnd Class\n\nPublic Class Inline Inherits BasePerson Implements IGreeter\nEnd Class\n"
    );
    let extracted = extract("src/Person.vb", &source);

    let greeter = symbol(&extracted, SymbolKind::Interface, "IGreeter");
    let greet = symbol(&extracted, SymbolKind::Method, "IGreeter::Greet");
    assert!(greet.implementation.declaration_only);
    assert_containment(&extracted, greeter, greet);
    let point = symbol(&extracted, SymbolKind::Struct, "Pt");
    let x = symbol(&extracted, SymbolKind::Field, "Pt::X");
    assert_containment(&extracted, point, x);
    let color = symbol(&extracted, SymbolKind::Enum, "Color");
    for member in ["Color::Red", "Color::Green"] {
        let member = symbol(&extracted, SymbolKind::EnumMember, member);
        assert_containment(&extracted, color, member);
    }

    let person = symbol(&extracted, SymbolKind::Class, "Person");
    assert_reference(&extracted, person, ("BasePerson", ReferenceKind::Extends));
    assert_reference(&extracted, person, ("IGreeter", ReferenceKind::Implements));
    assert_reference(
        &extracted,
        person,
        ("IDisposable", ReferenceKind::Implements),
    );
    let name = symbol(&extracted, SymbolKind::Field, "Person::_name");
    assert_eq!(name.visibility, Some(Visibility::Private));
    symbol(&extracted, SymbolKind::Field, "Person::x");
    symbol(&extracted, SymbolKind::Field, "Person::y");
    let greet = symbol(&extracted, SymbolKind::Method, "Person::Greet");
    assert_eq!(greet.visibility, Some(Visibility::Internal));
    assert!(greet.execution.static_member);
    assert_eq!(
        greet.signature.as_deref(),
        Some("(value As String) As String")
    );
    symbol(&extracted, SymbolKind::Variable, "Person::Greet::n");
    assert_reference(&extracted, greet, ("Helper", ReferenceKind::Calls));
    assert_reference(
        &extracted,
        greet,
        ("Console.WriteLine", ReferenceKind::Calls),
    );
    assert_reference(
        &extracted,
        greet,
        ("Formatter", ReferenceKind::Instantiates),
    );
    let f = symbol(&extracted, SymbolKind::Method, "Person::F");
    assert_reference(&extracted, f, ("Helper", ReferenceKind::Calls));
    symbol(&extracted, SymbolKind::Method, "Person::New");

    let inline = symbol(&extracted, SymbolKind::Class, "Inline");
    assert_reference(&extracted, inline, ("BasePerson", ReferenceKind::Extends));
    assert_reference(&extracted, inline, ("IGreeter", ReferenceKind::Implements));

    let names = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    for bogus in [
        "Person::BasePerson",
        "Person::IGreeter",
        "Person::IDisposable",
        "Person::Inherits",
        "Person::Implements",
    ] {
        assert!(!names.contains(bogus), "heritage became a member: {bogus}");
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::TypeAlias),
        "block types fell back to type aliases: {names:?}"
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "IGreeter.Greet"),
        "member Implements clause became a reference"
    );
    assert!(!format!("{extracted:?}").contains(LITERAL_SENTINEL));
}

#[test]
fn vbnet_namespaces_modules_and_declarations_qualify_their_members() {
    let extracted = extract(
        "src/Orders.vb",
        "Namespace Acme.Orders\n  Public Module Helpers\n    Public Const Limit As Integer = 5\n    Public Function Twice(value As Integer) As Integer\n      Return value * 2\n    End Function\n  End Module\n  Public Delegate Sub Notify(message As String)\nEnd Namespace\n",
    );
    let namespace = symbol(&extracted, SymbolKind::Namespace, "Acme.Orders");
    let module = symbol(&extracted, SymbolKind::Module, "Acme.Orders::Helpers");
    assert_containment(&extracted, namespace, module);
    symbol(
        &extracted,
        SymbolKind::Constant,
        "Acme.Orders::Helpers::Limit",
    );
    let twice = symbol(
        &extracted,
        SymbolKind::Method,
        "Acme.Orders::Helpers::Twice",
    );
    assert_eq!(
        twice.signature.as_deref(),
        Some("(value As Integer) As Integer")
    );
    let notify = symbol(&extracted, SymbolKind::TypeAlias, "Acme.Orders::Notify");
    assert_eq!(notify.signature.as_deref(), Some("(message As String)"));
}

#[test]
fn vbnet_heritage_recovery_survives_multibyte_text_before_fields() {
    // Multi-byte characters within the bounded recovery window must neither
    // fail extraction nor hide the heritage statement.
    let mut source = String::from("Public Class Greeting\n  ' ");
    source.push_str(&"ü".repeat(200));
    source.push_str("\n  Inherits Basis\n  Private size As Integer\nEnd Class\n");
    let extracted = extract("src/Gruesse.vb", &source);
    let class = symbol(&extracted, SymbolKind::Class, "Greeting");
    assert_reference(&extracted, class, ("Basis", ReferenceKind::Extends));
    symbol(&extracted, SymbolKind::Field, "Greeting::size");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.name != "Inherits" && symbol.name != "Basis"),
        "heritage became a member: {:?}",
        extracted.symbols
    );
}

#[test]
fn vbnet_heritage_lines_keep_qualified_generic_heads_and_abstain_on_malformed_clauses() {
    let extracted = extract(
        "src/Repo.vb",
        "Public Class Repo\n  Inherits Acme.Data.Base(Of Order)\n  Implements IReader, Acme.IWriter(Of Order, Key) ' trailing comment\nEnd Class\n\nPublic Class Broken\n  Implements 9Lives, Good\nEnd Class\n\nPublic Class Continued\n  Implements IFirst, _\n      ISecond\n  Sub Run()\n    Dim a, b As Integer\n    Dim c As Integer = 1, d As String\n  End Sub\nEnd Class\n",
    );
    let continued = symbol(&extracted, SymbolKind::Class, "Continued");
    assert_reference(&extracted, continued, ("IFirst", ReferenceKind::Implements));
    assert_reference(
        &extracted,
        continued,
        ("ISecond", ReferenceKind::Implements),
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "_"),
        "a line-continuation marker is not a type"
    );
    for (name, signature) in [
        ("Continued::Run::a", "a As Integer"),
        ("Continued::Run::b", "b As Integer"),
        ("Continued::Run::c", "c As Integer"),
        ("Continued::Run::d", "d As String"),
    ] {
        let local = symbol(&extracted, SymbolKind::Variable, name);
        assert_eq!(local.signature.as_deref(), Some(signature));
    }
    let repo = symbol(&extracted, SymbolKind::Class, "Repo");
    assert_reference(&extracted, repo, ("Acme.Data.Base", ReferenceKind::Extends));
    assert_reference(&extracted, repo, ("IReader", ReferenceKind::Implements));
    assert_reference(
        &extracted,
        repo,
        ("Acme.IWriter", ReferenceKind::Implements),
    );
    let broken = symbol(&extracted, SymbolKind::Class, "Broken");
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.owner.as_ref() != Some(&broken.id)
                || reference.kind != ReferenceKind::Implements),
        "a malformed clause must not be partially guessed: {:?}",
        extracted.references
    );
    for argument in ["Order", "Key", "comment"] {
        assert!(
            extracted
                .references
                .iter()
                .all(|reference| reference.name != argument),
            "{argument} is not a heritage target"
        );
    }
}

#[test]
fn vbnet_lists_chained_calls_and_declarators_keep_exact_identities() {
    let source = "Imports System, System.Text\n\nPublic Class Shop\n  Inherits Base(Of T) Extra\n  Implements IOpen(Of T\n  Public first As Integer, second As String\n  Sub Run()\n    Factory().Run(1)\n    Dim items = New List(Of Item)()\n    Dim inferred = GetValue(), typed As String\n  End Sub\nEnd Class\n\nPublic Class Partial\n  Implements IFoo, _\n      Broken(Of T\nEnd Class\n";
    let extracted = extract("src/Shop.vb", source);
    for module in ["System", "System.Text"] {
        symbol(&extracted, SymbolKind::Import, module);
    }
    let shop = symbol(&extracted, SymbolKind::Class, "Shop");
    assert!(
        extracted.references.iter().all(|reference| {
            reference.owner.as_ref() != Some(&shop.id)
                || !matches!(
                    reference.kind,
                    ReferenceKind::Extends | ReferenceKind::Implements
                )
        }),
        "trailing text or an unbalanced generic list must not yield heritage: {:?}",
        extracted.references
    );
    for (name, text) in [
        ("Shop::first", "first As Integer"),
        ("Shop::second", "second As String"),
    ] {
        let field = symbol(&extracted, SymbolKind::Field, name);
        let start = usize::try_from(field.span.start_byte()).unwrap_or(usize::MAX);
        let end = usize::try_from(field.span.end_byte()).unwrap_or(usize::MAX);
        assert_eq!(source.get(start..end), Some(text), "{name} span");
    }
    let run = symbol(&extracted, SymbolKind::Method, "Shop::Run");
    assert_reference(&extracted, run, ("Factory", ReferenceKind::Calls));
    assert_reference(&extracted, run, ("Run", ReferenceKind::Calls));
    assert_reference(&extracted, run, ("List", ReferenceKind::Instantiates));
    let factory_calls = extracted
        .references
        .iter()
        .filter(|reference| reference.name == "Factory")
        .count();
    assert_eq!(factory_calls, 1, "the receiver call is not reported twice");
    let inferred = symbol(&extracted, SymbolKind::Variable, "Shop::Run::inferred");
    assert_eq!(
        inferred.signature.as_deref(),
        Some("inferred"),
        "an initializer ends the shared As-clause group"
    );
    let typed = symbol(&extracted, SymbolKind::Variable, "Shop::Run::typed");
    assert_eq!(typed.signature.as_deref(), Some("typed As String"));
    let partial = symbol(&extracted, SymbolKind::Class, "Partial");
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.owner.as_ref() != Some(&partial.id)),
        "a malformed continued clause publishes nothing: {:?}",
        extracted.references
    );
}

#[test]
fn vbnet_colon_heritage_shared_field_types_and_any_case_self_receiver() {
    let extracted = extract(
        "src/Kid.vb",
        "Public Class Kid : Inherits Person\n  Public a, b As Integer\n  Sub Run()\n    me.Go()\n    ME.Halt()\n  End Sub\nEnd Class\nPublic Class Kid2 : Implements IA, IB\nEnd Class\nPublic Class Quoted\n  Dim s As String = \"x: Inherits Fake\"\nEnd Class\n",
    );
    let kid = symbol(&extracted, SymbolKind::Class, "Kid");
    assert_reference(&extracted, kid, ("Person", ReferenceKind::Extends));
    let kid2 = symbol(&extracted, SymbolKind::Class, "Kid2");
    assert_reference(&extracted, kid2, ("IA", ReferenceKind::Implements));
    assert_reference(&extracted, kid2, ("IB", ReferenceKind::Implements));
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "Person : Implements IFoo"),
        "a heritage statement ends at the next `:`"
    );
    assert!(
        extracted.symbols.iter().all(|symbol| !matches!(
            symbol.qualified_name.as_str(),
            "Kid::Inherits" | "Kid2::IA" | "Kid2::IB" | "Kid2::Implements"
        )),
        "a `:`-separated heritage clause is not a field: {:?}",
        extracted.symbols
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "Fake"),
        "a colon inside a string literal does not start a heritage statement"
    );
    for (name, signature) in [("Kid::a", "a As Integer"), ("Kid::b", "b As Integer")] {
        let field = symbol(&extracted, SymbolKind::Field, name);
        assert_eq!(field.signature.as_deref(), Some(signature), "{name}");
    }
    let run = symbol(&extracted, SymbolKind::Method, "Kid::Run");
    assert_reference(&extracted, run, ("Go", ReferenceKind::Calls));
    assert_reference(&extracted, run, ("Halt", ReferenceKind::Calls));
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.to_ascii_lowercase().starts_with("me.")),
        "VB keywords are case-insensitive: {:?}",
        extracted.references
    );
}

#[test]
fn vbnet_colon_heritage_ends_at_the_next_separator_and_needs_its_own_header() {
    let extracted = extract(
        "src/Chain.vb",
        "Public Class Kid : Inherits Person : Implements IFoo\nEnd Class\nPublic Class One : Inherits Base : End Class\nPublic Class Two\n  Inherits Person : Implements IBar\nEnd Class\nPublic Class Outer\n  Public Class Inner : Inherits Hidden\n  End Class\nEnd Class\nPublic Class Outer2 : Public Class Inner2 : Inherits Hidden\nEnd Class\nEnd Class\nPublic Class Three\n  Inherits Base : Public Class Inner3 : Implements Hidden\nEnd Class\nPublic Class Cont _\n  : Implements IFoo\nEnd Class\nPublic Class Four\n  Implements IA, _\n    IB : Implements IC\nEnd Class\nPublic Class Outer3\n  Public Class Inner4 : _\n    Inherits Hidden\n  End Class\nEnd Class\nPublic Class Five : _\n  Inherits Base : Implements IFive\nEnd Class\n",
    );
    let five = symbol(&extracted, SymbolKind::Class, "Five");
    assert_reference(&extracted, five, ("IFive", ReferenceKind::Implements));
    // Ownership follows ` _` continuations: the header and the clause form
    // one logical line.
    let cont = symbol(&extracted, SymbolKind::Class, "Cont");
    assert_reference(&extracted, cont, ("IFoo", ReferenceKind::Implements));
    let four = symbol(&extracted, SymbolKind::Class, "Four");
    for name in ["IA", "IB", "IC"] {
        assert_reference(&extracted, four, (name, ReferenceKind::Implements));
    }
    let kid = symbol(&extracted, SymbolKind::Class, "Kid");
    assert_reference(&extracted, kid, ("Person", ReferenceKind::Extends));
    assert_reference(&extracted, kid, ("IFoo", ReferenceKind::Implements));
    let one = symbol(&extracted, SymbolKind::Class, "One");
    assert_reference(&extracted, one, ("Base", ReferenceKind::Extends));
    let two = symbol(&extracted, SymbolKind::Class, "Two");
    assert_reference(&extracted, two, ("Person", ReferenceKind::Extends));
    assert_reference(&extracted, two, ("IBar", ReferenceKind::Implements));
    // The pinned grammar does not parse a nested `Class Inner : ...` header,
    // so the enclosing class stays the current owner: the inner heritage is
    // abstained on rather than given to it, even when the enclosing header
    // shares the line, and the statement is not a field either.
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "Hidden"),
        "a misparsed nested header leaked its heritage: {:?}",
        extracted.references
    );
    assert!(
        extracted.symbols.iter().all(|symbol| !matches!(
            symbol.qualified_name.as_str(),
            "Outer::Inherits" | "Two::Implements" | "Kid::Inherits"
        )),
        "heritage statements are not fields: {:?}",
        extracted.symbols
    );
}

#[test]
fn vbnet_continued_header_at_the_line_length_bound_keeps_its_heritage() {
    // A continued header of exactly the inspected line bound (1,024 bytes
    // without its newline) after another line still owns its clause.
    let header = format!("{:<1022} _", "Public Class Wide");
    assert_eq!(header.len(), 1_024);
    let extracted = extract(
        "src/Wide.vb",
        &format!("\n{header}\n  : Implements IWide\nEnd Class\n"),
    );
    let wide = symbol(&extracted, SymbolKind::Class, "Wide");
    assert_reference(&extracted, wide, ("IWide", ReferenceKind::Implements));
}

#[test]
fn vbnet_extraction_is_repeatable_and_cancellable() {
    let source = "Public Class Repeat\n  Sub Run()\n    Helper()\n  End Sub\nEnd Class\n";
    let first = extract("src/Repeat.vb", source);
    let second = extract("src/Repeat.vb", source);
    assert_eq!(first, second);
    let snapshot = snapshot("src/Repeat.vb", source);
    let mut extractor = NativeExtractor::new(SourceLanguage::VbNet)
        .unwrap_or_else(|error| panic!("VB.NET extractor failed: {error}"));
    assert_eq!(
        extractor
            .extract_with_cancellation(&snapshot, || true)
            .err(),
        Some(ExtractError::Cancelled)
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = snapshot(path, source);
    assert_eq!(snapshot.language(), SourceLanguage::VbNet);
    NativeExtractor::new(SourceLanguage::VbNet)
        .unwrap_or_else(|error| panic!("VB.NET extractor failed: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("VB.NET extraction failed for {path}: {error}"))
}

fn snapshot(path: &str, source: &str) -> SourceSnapshot {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"))
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name}; symbols={:?}",
                extracted
                    .symbols
                    .iter()
                    .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
                    .collect::<Vec<_>>()
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
        child.qualified_name
    );
}

fn assert_reference(
    extracted: &ExtractedFile,
    owner: &ExtractedSymbol,
    (name, kind): (&str, ReferenceKind),
) {
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&owner.id)
                && reference.name == name
                && reference.kind == kind
        }),
        "missing {kind:?} {name} owned by {}; references={:?}",
        owner.qualified_name,
        extracted
            .references
            .iter()
            .map(|reference| (reference.kind, reference.name.as_str()))
            .collect::<Vec<_>>()
    );
}
