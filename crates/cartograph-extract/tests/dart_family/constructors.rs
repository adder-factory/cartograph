use super::{ReferenceKind, SymbolKind, extract, symbol};
use cartograph_extract::DeclarationSyntax;

#[test]
fn constructor_redirects_emit_calls_and_keep_calls_inside_arguments() {
    let extracted = extract(
        "lib/model.dart",
        "int build() => 1; class Base { Base(int value); Base.named(int value); } class Child extends Base { Child(): super(build()); Child.named(): super.named(build()); Child.other(): this.named(); void ordinary() { print('super(fake())'); } }",
    );
    for (owner, name) in [
        ("Child", "super"),
        ("named", "super.named"),
        ("other", "this.named"),
    ] {
        let owner = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == format!("Child::{owner}"))
            .unwrap_or_else(|| panic!("missing constructor {owner}"));
        assert_eq!(owner.declaration_syntax, DeclarationSyntax::DartConstructor);
        let reference = extracted
            .references
            .iter()
            .find(|reference| reference.owner.as_ref() == Some(&owner.id) && reference.name == name)
            .unwrap_or_else(|| panic!("missing redirect {name}"));
        assert_eq!(reference.kind, ReferenceKind::Calls);
        assert!(
            reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.starts_with("dart-constructor-redirect:"))
        );
    }
    assert_eq!(
        extracted
            .references
            .iter()
            .filter(|reference| reference.name == "build")
            .count(),
        2
    );
    assert!(
        !extracted
            .references
            .iter()
            .any(|reference| reference.name == "fake")
    );
}

#[test]
fn extension_types_own_primary_and_body_constructors_without_impersonating_extensions() {
    let extracted = extract(
        "lib/id.dart",
        "extension type Id(int value) { Id.zero(): this(0); int get doubled => value * 2; } extension type Named._(int value) {} extension Plain on int { int twice() => this * 2; }",
    );
    for name in ["Id", "Named"] {
        symbol(&extracted, SymbolKind::Class, name);
    }
    for name in ["Id::Id", "Id::zero", "Named::_"] {
        let constructor = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", extracted.symbols));
        assert_eq!(
            constructor.declaration_syntax,
            DeclarationSyntax::DartConstructor
        );
    }
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "value").declaration_syntax,
        DeclarationSyntax::Other
    );
    assert!(
        extracted
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Field && symbol.qualified_name == "Id::value")
    );
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.qualified_name == "Plain::Plain")
    );
}

#[test]
fn factory_constructors_preserve_constructor_identity_with_static_receiver_context() {
    let extracted = extract(
        "lib/worker.dart",
        "class Worker { factory Worker.make() => Worker._(); factory Worker.redirect() = Worker._; Worker._(); }",
    );
    for name in ["make", "redirect", "_"] {
        let constructor = symbol(&extracted, SymbolKind::Method, name);
        assert_eq!(
            constructor.declaration_syntax,
            DeclarationSyntax::DartConstructor
        );
        assert_eq!(constructor.execution.static_member, name != "_");
    }
}
