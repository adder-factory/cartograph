//! F# extraction contracts ported from the v1 F# extractor scenarios.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, ImportBindingKind, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn fsharp_namespaces_modules_functions_records_and_calls() {
    let extracted = extract(
        "src/Sample.fs",
        r"
namespace Demo

module Math =
  let add x y = x + y
  type Person = { Name: string; Age: int }

let result = Math.add 1 2
",
    );
    assert_eq!(extracted.language, SourceLanguage::FSharp);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    let namespace = symbol(&extracted, SymbolKind::Namespace, "Demo");
    let module = symbol(&extracted, SymbolKind::Module, "Math");
    assert_eq!(module.qualified_name, "Demo::Math");
    assert!(contains(&extracted, &namespace.id, &module.id));
    let add = symbol(&extracted, SymbolKind::Function, "add");
    assert_eq!(add.qualified_name, "Demo::Math::add");
    assert!(add.export.exported);
    let person = symbol(&extracted, SymbolKind::Struct, "Person");
    assert_eq!(person.qualified_name, "Demo::Math::Person");
    for (field, signature) in [("Name", "Name: string"), ("Age", "Age: int")] {
        let field_symbol = symbol(&extracted, SymbolKind::Field, field);
        assert_eq!(
            field_symbol.qualified_name,
            format!("Demo::Math::Person::{field}")
        );
        assert_eq!(field_symbol.signature.as_deref(), Some(signature));
    }
    let result = symbol(&extracted, SymbolKind::Variable, "result");
    assert_eq!(result.qualified_name, "Demo::result");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !(symbol.name == "result" && symbol.kind == SymbolKind::Function)),
        "a value binding is not a function"
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&result.id)
                && reference.kind == ReferenceKind::Calls
                && reference.name == "Math.add"
        }),
        "{:?}",
        extracted.references
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::TypeAlias),
        "records are structs, not aliases"
    );
}

#[test]
fn fsharp_open_declarations_are_imports_with_namespace_bindings() {
    let extracted = extract(
        "src/Program.fs",
        r"
module Program

open System
open System.Collections.Generic

let f x = helper x
",
    );
    let program = symbol(&extracted, SymbolKind::Module, "Program");
    for module in ["System", "System.Collections.Generic"] {
        symbol(&extracted, SymbolKind::Import, module);
        assert!(extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Imports && reference.name == module
        }));
        assert!(extracted.import_bindings.iter().any(|binding| {
            binding.kind == ImportBindingKind::Namespace
                && binding.module_specifier == module
                && binding.local_name == "*"
        }));
    }
    let function = symbol(&extracted, SymbolKind::Function, "f");
    assert_eq!(function.qualified_name, "Program::f");
    assert!(contains(&extracted, &program.id, &function.id));
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&function.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "helper"
    }));
}

#[test]
fn fsharp_type_kinds_enum_cases_and_member_names() {
    let extracted = extract(
        "src/Shapes.fs",
        r#"
module Shapes

type Color =
  | Red = 0
  | Green = 1

type Shape =
  | Circle of float
  | Square of float

type IGreeter =
  interface
    abstract Greet : string -> string
  end

type Greeter(name: string) =
  member this.Greet(other: string) = sprintf "%s %s" name other
  static member Create() = Greeter("x")
  member private this.Hidden() = ()
"#,
    );
    let color = symbol(&extracted, SymbolKind::Enum, "Color");
    for case in ["Red", "Green"] {
        let member = symbol(&extracted, SymbolKind::EnumMember, case);
        assert!(contains(&extracted, &color.id, &member.id));
        assert!(
            member.signature.is_none(),
            "enum case values are literals and must not leak"
        );
    }
    symbol(&extracted, SymbolKind::Union, "Shape");
    symbol(&extracted, SymbolKind::Interface, "IGreeter");
    let greeter = symbol(&extracted, SymbolKind::Class, "Greeter");
    let greet = symbol(&extracted, SymbolKind::Method, "Greet");
    assert_eq!(greet.qualified_name, "Shapes::Greeter::Greet");
    assert!(contains(&extracted, &greeter.id, &greet.id));
    assert_eq!(greet.visibility, Some(Visibility::Public));
    let create = symbol(&extracted, SymbolKind::Method, "Create");
    assert!(create.execution.static_member);
    let hidden = symbol(&extracted, SymbolKind::Method, "Hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert!(
        extracted.symbols.iter().all(|symbol| symbol.name != "this"),
        "the self identifier is never a member name: {:?}",
        extracted.symbols
    );
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&greet.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "sprintf"
    }));
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&create.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "Greeter"
    }));
}

#[test]
fn fsharp_calls_skip_builders_operators_and_curried_partials() {
    let extracted = extract(
        "src/Calls.fs",
        r#"
module Calls

let private g = async { return 1 }
let h () = printfn "hi"; ignore 3
let pipeline xs = xs |> List.map f
let nested a = outer (inner a) a
let typed x = helper<byte> x
let backward x = List.sum <| List.map f x
"#,
    );
    let g = symbol(&extracted, SymbolKind::Variable, "g");
    assert_eq!(g.visibility, Some(Visibility::Private));
    assert!(!g.export.exported);
    let calls = |owner: &str| {
        let owner = symbol(&extracted, SymbolKind::Function, owner);
        let mut names = extracted
            .references
            .iter()
            .filter(|reference| {
                reference.owner.as_ref() == Some(&owner.id)
                    && reference.kind == ReferenceKind::Calls
            })
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    };
    assert_eq!(calls("h"), ["printfn"]);
    assert_eq!(
        calls("pipeline"),
        ["List.map"],
        "the piped function is called"
    );
    assert_eq!(
        calls("typed"),
        ["helper"],
        "type arguments never enter a call name"
    );
    assert_eq!(calls("nested"), ["inner", "outer"]);
    assert!(
        extracted.references.iter().all(|reference| {
            reference.kind != ReferenceKind::Calls
                || !matches!(reference.name.as_str(), "async" | "ignore")
        }),
        "{:?}",
        extracted.references
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.contains('"') && !reference.name.contains(' ')),
        "{:?}",
        extracted.references
    );
}

#[test]
fn fsharp_extraction_is_deterministic() {
    let source =
        "namespace Demo\nmodule Math =\n  let add x y = x + y\nlet result = Math.add 1 2\n";
    let first = extract("src/Sample.fs", source);
    assert_eq!(first, extract("src/Sample.fs", source));
    assert_eq!(
        symbol(&first, SymbolKind::Function, "add").qualified_name,
        "Demo::Math::add"
    );
}

#[test]
fn fsharp_recursive_groups_and_access_modifiers() {
    let extracted = extract(
        "src/Groups.fs",
        r"
module Groups

let rec f x = first x
and g x = second x

type private Hidden = { X: int }

type Shown =
  private { Y: int }

module internal Inner =
  let h = 1

type Commented = { Name: (* sk_live_fsharp_sentinel *) string }
",
    );
    for (function, call) in [("f", "first"), ("g", "second")] {
        let symbol = symbol(&extracted, SymbolKind::Function, function);
        assert!(
            extracted.references.iter().any(|reference| {
                reference.owner.as_ref() == Some(&symbol.id)
                    && reference.kind == ReferenceKind::Calls
                    && reference.name == call
            }),
            "{function} must own {call}: {:?}",
            extracted.references
        );
    }
    let f = symbol(&extracted, SymbolKind::Function, "f");
    let g = symbol(&extracted, SymbolKind::Function, "g");
    assert!(
        f.span.end_byte() <= g.span.start_byte(),
        "group members span their own bindings"
    );
    let hidden = symbol(&extracted, SymbolKind::Struct, "Hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert!(!hidden.export.exported);
    let shown = symbol(&extracted, SymbolKind::Struct, "Shown");
    assert_eq!(shown.visibility, Some(Visibility::Public));
    assert!(
        shown.export.exported,
        "a private representation keeps the type public"
    );
    let inner = symbol(&extracted, SymbolKind::Module, "Inner");
    assert_eq!(inner.visibility, Some(Visibility::Internal));
    assert!(!inner.export.exported);
    let name = symbol(&extracted, SymbolKind::Field, "Name");
    assert!(name.signature.is_none());
    assert!(!format!("{:?}", extracted.symbols).contains("sk_live_fsharp_sentinel"));
}

#[test]
fn fsharp_names_skip_comments_and_group_digests_follow_bodies() {
    let extracted = extract(
        "src/Commented.fs",
        "module Demo.(* sk_live_fsharp_module *)Inner\nopen System.(* sk_live_fsharp_open *)IO\nlet value = 1\n",
    );
    let module = symbol(&extracted, SymbolKind::Module, "Demo.Inner");
    assert_eq!(module.qualified_name, "Demo.Inner");
    symbol(&extracted, SymbolKind::Import, "System.IO");
    assert!(
        !format!("{:?}", extracted.symbols).contains("sk_live"),
        "{:?}",
        extracted.symbols
    );

    let first = extract(
        "src/Groups.fs",
        "module Groups\nlet rec f x = first x\nand g x = second x\n",
    );
    let second = extract(
        "src/Groups.fs",
        "module Groups\nlet rec f x = first x\nand g x = if x then second x else third x\n",
    );
    assert_eq!(
        symbol(&first, SymbolKind::Function, "f").structural_digest,
        symbol(&second, SymbolKind::Function, "f").structural_digest
    );
    assert_ne!(
        symbol(&first, SymbolKind::Function, "g").structural_digest,
        symbol(&second, SymbolKind::Function, "g").structural_digest,
        "a grouped binding's digest must follow its own body"
    );
}

#[test]
fn fsharp_generic_types_are_named_and_own_their_members() {
    let extracted = extract(
        "src/Generic.fs",
        r"
module M

type Box<'T> = { Value: 'T; Label: string }
type Res<'a> = | Ok of 'a | Err
type Wrapper<'T>(v: 'T) =
  member _.Get() = v
type private Hidden<'T> = { Item: 'T }
",
    );
    let module = symbol(&extracted, SymbolKind::Module, "M");
    let boxed = symbol(&extracted, SymbolKind::Struct, "Box");
    assert_eq!(boxed.qualified_name, "M::Box");
    assert!(contains(&extracted, &module.id, &boxed.id));
    for field in ["Value", "Label"] {
        let field_symbol = symbol(&extracted, SymbolKind::Field, field);
        assert_eq!(field_symbol.qualified_name, format!("M::Box::{field}"));
        assert!(contains(&extracted, &boxed.id, &field_symbol.id));
    }
    let result = symbol(&extracted, SymbolKind::Union, "Res");
    assert_eq!(result.qualified_name, "M::Res");
    let wrapper = symbol(&extracted, SymbolKind::Class, "Wrapper");
    assert_eq!(wrapper.qualified_name, "M::Wrapper");
    let get = symbol(&extracted, SymbolKind::Method, "Get");
    assert_eq!(get.qualified_name, "M::Wrapper::Get");
    assert!(contains(&extracted, &wrapper.id, &get.id));
    let hidden = symbol(&extracted, SymbolKind::Struct, "Hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert!(!hidden.export.exported);
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !symbol.name.contains('<') && !symbol.name.contains('\'')),
        "type parameters never enter symbol names: {:?}",
        extracted.symbols
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.qualified_name != "M::Value" && symbol.qualified_name != "M::Get"),
        "generic type members must not leak into the module scope"
    );
}

#[test]
fn fsharp_recursive_namespaces_keep_their_dotted_name() {
    let extracted = extract(
        "src/Rec.fs",
        r"
namespace rec Outer.Inner

type Node = { Next: Node option }
",
    );
    let namespace = symbol(&extracted, SymbolKind::Namespace, "Outer.Inner");
    let node = symbol(&extracted, SymbolKind::Struct, "Node");
    assert_eq!(node.qualified_name, "Outer.Inner::Node");
    assert!(contains(&extracted, &namespace.id, &node.id));
    assert!(
        extracted.symbols.iter().all(|symbol| symbol.name != "rec"),
        "the rec keyword never names a namespace"
    );
}

#[test]
fn fsharp_private_modules_hide_their_declarations() {
    let extracted = extract(
        "src/Hidden.fs",
        r"
module Outer

module private Inner =
  let helper x = x
  type Shape = { Side: int }
  module Deeper =
    let nested y = y

module Open =
  let visible z = z
",
    );
    for (kind, name) in [
        (SymbolKind::Function, "helper"),
        (SymbolKind::Struct, "Shape"),
        (SymbolKind::Module, "Deeper"),
        (SymbolKind::Function, "nested"),
    ] {
        let hidden = symbol(&extracted, kind, name);
        assert!(
            !hidden.export.exported,
            "{name} sits in a private module and is not exported"
        );
    }
    let inner = symbol(&extracted, SymbolKind::Module, "Inner");
    assert!(!inner.export.exported);
    assert_eq!(inner.visibility, Some(Visibility::Private));
    let visible = symbol(&extracted, SymbolKind::Function, "visible");
    assert_eq!(visible.qualified_name, "Outer::Open::visible");
    assert!(visible.export.exported);
    assert_eq!(visible.visibility, Some(Visibility::Public));
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
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

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| {
            let available = extracted
                .symbols
                .iter()
                .map(|symbol| format!("{:?} {}", symbol.kind, symbol.qualified_name))
                .collect::<Vec<_>>();
            panic!("missing {kind:?} {name}; extracted: {available:?}")
        })
}
