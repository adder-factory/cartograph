//! Dart extraction contracts ported from the v1 Dart extractor scenarios.

#[path = "dart_family/constructors.rs"]
mod constructors;
mod credential_support;
mod dependency_ownership;
#[path = "credential_support/escaped_specifiers.rs"]
mod escaped_specifiers;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, ImportBindingKind, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn dart_classes_own_async_and_private_methods_with_bodies() {
    let extracted = extract(
        "lib/service.dart",
        r"
class UserService {
  final Database _db;

  Future<User> findById(String id) async {
    return await _db.query(id);
  }

  void _privateMethod() {}
}
",
    );
    assert_eq!(extracted.language, SourceLanguage::Dart);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    let class = symbol(&extracted, SymbolKind::Class, "UserService");
    assert_eq!(class.visibility, Some(Visibility::Public));
    assert!(class.export.exported);

    let find_by_id = symbol(&extracted, SymbolKind::Method, "findById");
    assert_eq!(find_by_id.qualified_name, "UserService::findById");
    assert!(find_by_id.execution.async_symbol);
    assert!(!find_by_id.implementation.declaration_only);
    assert_eq!(
        find_by_id.signature.as_deref(),
        Some("Future<User> (String id)")
    );
    assert!(
        find_by_id.span.end_line() > find_by_id.span.start_line(),
        "the method span must include its sibling function_body"
    );
    let private_method = symbol(&extracted, SymbolKind::Method, "_privateMethod");
    assert_eq!(private_method.visibility, Some(Visibility::Private));
    assert!(!private_method.execution.async_symbol);
    assert!(contains(&extracted, &class.id, &private_method.id));

    let field = symbol(&extracted, SymbolKind::Field, "_db");
    assert_eq!(field.qualified_name, "UserService::_db");
    assert_eq!(field.signature.as_deref(), Some("Database _db"));
    assert_eq!(field.visibility, Some(Visibility::Private));

    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&find_by_id.id)
                && reference.kind == ReferenceKind::Calls
                && reference.name == "_db.query"
        }),
        "{:?}",
        extracted.references
    );
}

#[test]
fn dart_top_level_functions_privacy_async_and_static_members() {
    let extracted = extract(
        "lib/utils.dart",
        r"
void topLevelFunction(String name) {
  print(name);
}

Future<String> fetchData() async {
  return await http.get('/data');
}

void _privateHelper() {}

void publicFunction() {}

class Utils {
  static void doWork() {}
  static int count = 0, total = 1;
}
",
    );
    let top_level = symbol(&extracted, SymbolKind::Function, "topLevelFunction");
    assert_eq!(top_level.qualified_name, "topLevelFunction");
    assert_eq!(top_level.signature.as_deref(), Some("void (String name)"));
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&top_level.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "print"
    }));
    let fetch = symbol(&extracted, SymbolKind::Function, "fetchData");
    assert!(fetch.execution.async_symbol);
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&fetch.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "http.get"
    }));
    let private_helper = symbol(&extracted, SymbolKind::Function, "_privateHelper");
    assert_eq!(private_helper.visibility, Some(Visibility::Private));
    assert!(!private_helper.export.exported);
    let public_function = symbol(&extracted, SymbolKind::Function, "publicFunction");
    assert_eq!(public_function.visibility, Some(Visibility::Public));
    assert!(public_function.export.exported);

    let do_work = symbol(&extracted, SymbolKind::Method, "doWork");
    assert!(do_work.execution.static_member);
    for name in ["count", "total"] {
        let field = symbol(&extracted, SymbolKind::Field, name);
        assert!(field.execution.static_member, "{name}");
        assert_eq!(field.qualified_name, format!("Utils::{name}"));
    }
    assert!(
        extracted.references.iter().all(|reference| {
            !reference.name.contains('\'') && !reference.name.contains("/data")
        }),
        "string literals must never become reference names: {:?}",
        extracted.references
    );
}

#[test]
fn dart_enums_mixins_extensions_and_heritage_are_structural() {
    let extracted = extract(
        "lib/models.dart",
        r"
enum Status { active, inactive, pending }

mixin LoggerMixin on Base {
  void log(String message) {}
}

extension StringExt on String {
  bool get isBlank => trim().isEmpty;
}

class Widget extends Base with LoggerMixin implements Drawable, Sized {}
",
    );
    let status = symbol(&extracted, SymbolKind::Enum, "Status");
    for member in ["active", "inactive", "pending"] {
        let constant = symbol(&extracted, SymbolKind::EnumMember, member);
        assert_eq!(constant.qualified_name, format!("Status::{member}"));
        assert!(contains(&extracted, &status.id, &constant.id));
    }
    let mixin = symbol(&extracted, SymbolKind::Class, "LoggerMixin");
    let log = symbol(&extracted, SymbolKind::Method, "log");
    assert_eq!(log.qualified_name, "LoggerMixin::log");
    assert!(contains(&extracted, &mixin.id, &log.id));
    let extension = symbol(&extracted, SymbolKind::Class, "StringExt");
    let is_blank = symbol(&extracted, SymbolKind::Method, "isBlank");
    assert!(contains(&extracted, &extension.id, &is_blank.id));
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&is_blank.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "trim"
    }));

    let widget = symbol(&extracted, SymbolKind::Class, "Widget");
    for (kind, name) in [
        (ReferenceKind::Extends, "Base"),
        (ReferenceKind::Inherits, "LoggerMixin"),
        (ReferenceKind::Implements, "Drawable"),
        (ReferenceKind::Implements, "Sized"),
    ] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.owner.as_ref() == Some(&widget.id)
                    && reference.kind == kind
                    && reference.name == name
            }),
            "missing {kind:?} {name}: {:?}",
            extracted.references
        );
    }
    assert!(
        !extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&mixin.id) && reference.name == "Base"
        }),
        "a mixin `on` constraint is not inheritance"
    );
}

#[test]
fn dart_imports_and_exports_are_named_by_uri_with_bindings() {
    let extracted = extract(
        "lib/main.dart",
        r"
import 'dart:async';
import 'dart:convert';
import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;
import '../utils/helpers.dart';
export 'src/foo.dart';
export 'src/bar.dart' show Bar, Baz;
export 'src/qux.dart' hide Hidden;
",
    );
    let imports = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Import)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        imports,
        [
            "dart:async",
            "dart:convert",
            "package:flutter/material.dart",
            "package:http/http.dart",
            "../utils/helpers.dart",
            "src/foo.dart",
            "src/bar.dart",
            "src/qux.dart",
        ]
    );
    for module in imports {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Imports
                    && reference.name == module
                    && reference.owner.is_none()
            }),
            "missing import reference {module}"
        );
    }
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Namespace
            && binding.module_specifier == "package:http/http.dart"
            && binding.local_name == "http"
    }));
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Namespace
            && binding.module_specifier == "../utils/helpers.dart"
            && binding.local_name == "*"
    }));
    for module in ["src/foo.dart", "src/qux.dart"] {
        assert!(extracted.import_bindings.iter().any(|binding| {
            binding.kind == ImportBindingKind::ReExportAll && binding.module_specifier == module
        }));
    }
    let shown = extracted
        .import_bindings
        .iter()
        .filter(|binding| binding.module_specifier == "src/bar.dart")
        .map(|binding| (binding.kind, binding.imported_name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        shown,
        [
            (ImportBindingKind::ReExportNamed, "Bar"),
            (ImportBindingKind::ReExportNamed, "Baz"),
        ],
        "`show` limits a re-export to the shown names"
    );
}

#[test]
fn dart_calls_and_constructions_name_their_targets_once() {
    let extracted = extract(
        "lib/app.dart",
        r"
void main() {
  final x = helper(1);
  bar.doIt(x);
  service.load().then(done);
  var w = new MyWidget();
  var e = const EdgeInsets.all(8);
  runApp(App());
}
",
    );
    let main = symbol(&extracted, SymbolKind::Function, "main");
    let owned = |kind: ReferenceKind| {
        let mut names = extracted
            .references
            .iter()
            .filter(|reference| {
                reference.owner.as_ref() == Some(&main.id) && reference.kind == kind
            })
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    };
    assert_eq!(
        owned(ReferenceKind::Calls),
        [
            "App",
            "bar.doIt",
            "helper",
            "runApp",
            "service.load",
            "then"
        ]
    );
    assert_eq!(
        owned(ReferenceKind::Instantiates),
        ["EdgeInsets", "MyWidget"]
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !matches!(symbol.name.as_str(), "x" | "w" | "e")),
        "function locals are not symbols: {:?}",
        extracted.symbols
    );
}

#[test]
fn dart_abstract_external_and_accessor_members_are_declarations() {
    let extracted = extract(
        "lib/shape.dart",
        r"
abstract class Shape {
  double area();
  external void native();
  int get sides => 0;
  set sides(int value) {}
}

final topVar = defaultValue;
const int limit = 42;
typedef int Compare(Object a, Object b);
typedef IntList = List<int>;
",
    );
    for alias in ["Compare", "IntList"] {
        assert_eq!(
            symbol(&extracted, SymbolKind::TypeAlias, alias).qualified_name,
            alias
        );
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !(symbol.kind == SymbolKind::TypeAlias && symbol.name == "int")),
        "a legacy typedef's return type is not its name"
    );
    let area = symbol(&extracted, SymbolKind::Method, "area");
    assert!(area.implementation.declaration_only);
    let native = symbol(&extracted, SymbolKind::Method, "native");
    assert!(native.implementation.declaration_only);
    let accessors = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "sides")
        .count();
    assert_eq!(accessors, 2, "getter and setter are both members");
    let top_var = symbol(&extracted, SymbolKind::Variable, "topVar");
    assert_eq!(top_var.qualified_name, "topVar");
    let limit = symbol(&extracted, SymbolKind::Constant, "limit");
    assert_eq!(limit.qualified_name, "limit");
    assert!(
        limit.signature.is_none(),
        "literal initializers must not leak into signatures"
    );
}

#[test]
fn dart_extraction_is_deterministic_and_never_emits_a_compilation_unit_module() {
    let source = "class Box<T> { final T value; Box(this.value); }\nint size() => 1;\n";
    let first = extract("lib/models.dart", source);
    let second = extract("lib/models.dart", source);
    assert_eq!(first, second);
    assert!(
        first
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Module),
        "{:?}",
        first.symbols
    );
    let size = symbol(&first, SymbolKind::Function, "size");
    assert_eq!(size.qualified_name, "size");
    let value = symbol(&first, SymbolKind::Field, "value");
    assert_eq!(value.qualified_name, "Box::value");
}

#[test]
fn dart_bodies_attach_once_across_comments_constructors_and_operators() {
    let extracted = extract(
        "lib/point.dart",
        r"
class Point {
  Point(this.x) { init(); }
  Point operator +(Point other) => combine(other);
  void move() /* trailing */ {
    step();
    step();
  }
  static const origin = zero;
}
",
    );
    let point = symbol(&extracted, SymbolKind::Class, "Point");
    let movement = symbol(&extracted, SymbolKind::Method, "move");
    assert!(!movement.implementation.declaration_only);
    let steps = extracted
        .references
        .iter()
        .filter(|reference| reference.name == "step")
        .collect::<Vec<_>>();
    assert_eq!(steps.len(), 2, "each call site is one reference: {steps:?}");
    for step in steps {
        assert_eq!(step.owner.as_ref(), Some(&movement.id));
        assert!(
            movement.span.start_byte() <= step.span.start_byte()
                && step.span.end_byte() <= movement.span.end_byte(),
            "a body reference must lie inside its owner's span"
        );
    }
    let constructor = symbol(&extracted, SymbolKind::Method, "Point");
    for (callee, owner) in [("init", &constructor.id), ("combine", &point.id)] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.owner.as_ref() == Some(owner) && reference.name == callee
            }),
            "calls in constructor and operator bodies must survive: {callee}"
        );
    }
    let origin = symbol(&extracted, SymbolKind::Field, "origin");
    assert!(origin.execution.static_member);
    assert_eq!(origin.qualified_name, "Point::origin");
}

#[test]
fn dart_factory_constructors_use_the_declared_constructor_name() {
    let extracted = extract(
        "lib/user.dart",
        r"
class User {
  User(this.name);
  factory User.fromJson(Map<String, dynamic> json) {
    return User(json);
  }
  factory User.empty() => build();
  const factory User.redirect(int a) = User;
}
",
    );
    let user = symbol(&extracted, SymbolKind::Class, "User");
    let factories = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .collect::<Vec<_>>();
    assert_eq!(
        factories
            .iter()
            .map(|factory| (factory.qualified_name.as_str(), factory.span.start_line()))
            .collect::<Vec<_>>(),
        [
            ("User::User", 3),
            ("User::fromJson", 4),
            ("User::empty", 7),
            ("User::redirect", 8)
        ],
        "{:?}",
        extracted.symbols
    );
    let from_json = factories[1];
    assert!(contains(&extracted, &user.id, &from_json.id));
    assert_eq!(
        from_json.signature.as_deref(),
        Some("(Map<String, dynamic> json)")
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.name == "User" && reference.owner.as_ref() == Some(&from_json.id)
        }),
        "a factory owns its body's calls: {:?}",
        extracted.references
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.name == "build" && reference.owner.as_ref() == Some(&factories[2].id)
        }),
        "an expression-bodied factory owns its body too"
    );
}

#[test]
fn dart_parity_corpus_emits_generative_and_named_factory_constructors() {
    let file = extract(
        "lib/models/user.dart",
        include_str!("fixtures/v1_parity/dart/lib/models/user.dart"),
    );
    let user = symbol(&file, SymbolKind::Class, "User");
    for (name, qualified, start, end) in [
        ("User", "User::User", 40, 40),
        ("guest", "User::guest", 42, 42),
        ("fromJson", "User::fromJson", 44, 46),
    ] {
        let constructor = symbol(&file, SymbolKind::Method, name);
        assert_eq!(constructor.qualified_name, qualified);
        assert_eq!(constructor.span.start_line(), start);
        assert_eq!(constructor.span.end_line(), end);
        assert!(contains(&file, &user.id, &constructor.id));
    }
    let role = symbol(&file, SymbolKind::Enum, "Role");
    let constructor = symbol(&file, SymbolKind::Method, "Role");
    assert_eq!(constructor.qualified_name, "Role::Role");
    assert_eq!(constructor.span.start_line(), 13);
    assert!(contains(&file, &role.id, &constructor.id));
}

#[test]
fn dart_constructor_siblings_preserve_names_containment_and_initializer_calls() {
    let file = extract(
        "lib/constructors.dart",
        "class User {\n  User(this.name) { init(); }\n  User.guest() : name = makeName();\n  User.alias(String name) : this(name);\n  const User.zero() : name = '';\n  factory User.fromJson(Map<String, dynamic> json) { return User(readName(json)); }\n  factory User.empty() => build();\n  const factory User.redirect() = User.zero;\n  external User.external(String name);\n  external factory User.externalFactory(String name);\n  User._private() : name = '';\n  final String name;\n}\n",
    );
    let class = symbol(&file, SymbolKind::Class, "User");
    for (name, line) in [
        ("User", 2),
        ("guest", 3),
        ("alias", 4),
        ("zero", 5),
        ("fromJson", 6),
        ("empty", 7),
        ("redirect", 8),
        ("external", 9),
        ("externalFactory", 10),
        ("_private", 11),
    ] {
        let constructor = symbol(&file, SymbolKind::Method, name);
        assert_eq!(constructor.qualified_name, format!("User::{name}"));
        assert_eq!(constructor.span.start_line(), line);
        assert_eq!(constructor.span.end_line(), line);
        assert!(contains(&file, &class.id, &constructor.id));
    }
    assert_eq!(
        file.symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Method)
            .count(),
        10
    );
    for (callee, owner) in [("init", "User"), ("makeName", "guest"), ("build", "empty")] {
        let constructor = symbol(&file, SymbolKind::Method, owner);
        assert!(file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == callee
                && reference.owner.as_ref() == Some(&constructor.id)
        }));
    }
    let private = symbol(&file, SymbolKind::Method, "_private");
    assert_eq!(private.visibility, Some(Visibility::Private));
    let redirect = symbol(&file, SymbolKind::Method, "redirect");
    assert_eq!(redirect.signature.as_deref(), Some("()"));
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn dart_unnamed_const_factory_and_external_constructor_siblings() {
    for declaration in [
        "const User();",
        "factory User() => build();",
        "factory User() = Other;",
        "external User();",
        "external factory User();",
    ] {
        let file = extract("lib/user.dart", &format!("class User {{ {declaration} }}"));
        let class = symbol(&file, SymbolKind::Class, "User");
        let constructor = symbol(&file, SymbolKind::Method, "User");
        assert_eq!(constructor.qualified_name, "User::User");
        assert_eq!(constructor.signature.as_deref(), Some("()"));
        assert!(contains(&file, &class.id, &constructor.id));
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn dart_constructor_default_values_keep_their_construction_uses() {
    let file = extract(
        "lib/user.dart",
        "class User { const User({this.flag = const Flag()}); final Flag flag; }",
    );
    let constructor = symbol(&file, SymbolKind::Method, "User");
    assert!(file.references.iter().any(|reference| {
        reference.kind == ReferenceKind::Instantiates
            && reference.name == "Flag"
            && reference.owner.as_ref() == Some(&constructor.id)
    }));
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn dart_callable_digests_follow_the_body_not_only_the_signature() {
    let first = extract("lib/a.dart", "void run() { alpha(); }\n");
    let second = extract("lib/a.dart", "void run() { if (ready) { beta(); } }\n");
    let first_run = symbol(&first, SymbolKind::Function, "run");
    let second_run = symbol(&second, SymbolKind::Function, "run");
    assert_eq!(first_run.signature, second_run.signature);
    assert_ne!(first_run.structural_digest, second_run.structural_digest);
    assert_ne!(first_run.clone_shape_digest, second_run.clone_shape_digest);
}

#[test]
fn dart_concrete_constructor_forms_are_implementations_and_external_forms_are_declarations() {
    let file = extract(
        "lib/box.dart",
        "class Box<T> {\n  T value;\n  Box(this.value);\n  Box.initialized() : value = makeValue();\n  const Box.constant(this.value);\n  const Box.zero() : value = zero;\n  Box.alias(T value) : this(value);\n  factory Box.redirect(T value) = Other;\n  const factory Box.constRedirect(T value) = Other;\n  external Box.external(T value);\n  external factory Box.externalFactory(T value);\n  external const Box.externalConst(T value);\n}\n",
    );
    for name in [
        "Box",
        "initialized",
        "constant",
        "zero",
        "alias",
        "redirect",
        "constRedirect",
    ] {
        let constructor = symbol(&file, SymbolKind::Method, name);
        assert!(!constructor.implementation.declaration_only, "{name}");
    }
    for name in ["external", "externalFactory", "externalConst"] {
        let constructor = symbol(&file, SymbolKind::Method, name);
        assert!(constructor.implementation.declaration_only, "{name}");
    }
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn dart_constructor_initializer_edits_change_digest_and_executable_evidence() {
    for body in [" {}", ";"] {
        let first = extract(
            "lib/user.dart",
            &format!("class User {{ final Object name; User() : name = alpha(){body} }}"),
        );
        let second = extract(
            "lib/user.dart",
            &format!("class User {{ final Object name; User() : name = beta(){body} }}"),
        );
        let alpha = symbol(&first, SymbolKind::Method, "User");
        let beta = symbol(&second, SymbolKind::Method, "User");
        assert_eq!(alpha.id, beta.id);
        assert_eq!(alpha.signature, beta.signature);
        assert_ne!(alpha.structural_digest, beta.structural_digest);
        for (file, constructor, callee) in [(&first, alpha, "alpha"), (&second, beta, "beta")] {
            assert!(
                constructor
                    .body_search_text
                    .split_whitespace()
                    .any(|token| token == callee)
            );
            assert!(file.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Calls
                    && reference.name == callee
                    && reference.owner.as_ref() == Some(&constructor.id)
            }));
        }
    }
}

#[test]
fn dart_review_edges_prefixed_heritage_initializers_trivia_and_privacy() {
    let extracted = extract(
        "lib/edges.dart",
        r"
import 'package:x/x.dart' as pkg;
export 'c.dart' show X, Y hide Y;

class C extends pkg.Base<int> with pkg.M implements pkg.I, J<T> {
  final int x;
  C() : x = seed() { body(); }
  void run(/* sk_live_dart_sentinel */ int value) {
    helper /* note */ ();
    obj /* note */ .go();
  }
}

extension on String {
  void clean() { scrub(); }
}
",
    );
    let class = symbol(&extracted, SymbolKind::Class, "C");
    let mut heritage = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.owner.as_ref() == Some(&class.id)
                && matches!(
                    reference.kind,
                    ReferenceKind::Extends | ReferenceKind::Inherits | ReferenceKind::Implements
                )
        })
        .map(|reference| (reference.kind, reference.name.as_str()))
        .collect::<Vec<_>>();
    heritage.sort_unstable_by_key(|(_, name)| *name);
    assert_eq!(
        heritage,
        [
            (ReferenceKind::Implements, "J"),
            (ReferenceKind::Extends, "pkg.Base"),
            (ReferenceKind::Implements, "pkg.I"),
            (ReferenceKind::Inherits, "pkg.M"),
        ]
    );
    let constructor = symbol(&extracted, SymbolKind::Method, "C");
    for call in ["seed", "body"] {
        let owned = extracted
            .references
            .iter()
            .filter(|reference| reference.name == call)
            .collect::<Vec<_>>();
        assert_eq!(owned.len(), 1, "{call}: {owned:?}");
        assert_eq!(owned[0].owner.as_ref(), Some(&constructor.id), "{call}");
    }
    let run = symbol(&extracted, SymbolKind::Method, "run");
    assert!(run.signature.is_none(), "{:?}", run.signature);
    assert!(!format!("{:?}", extracted.symbols).contains("sk_live_dart_sentinel"));
    for call in ["helper", "obj.go"] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.owner.as_ref() == Some(&run.id) && reference.name == call
            }),
            "comments must not break the call {call}: {:?}",
            extracted.references
        );
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.name != "clean"),
        "an anonymous extension member is not a free function: {:?}",
        extracted.symbols
    );
    assert!(
        extracted
            .references
            .iter()
            .any(|reference| reference.name == "scrub")
    );
    let shown = extracted
        .import_bindings
        .iter()
        .filter(|binding| binding.module_specifier == "c.dart")
        .map(|binding| binding.imported_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(shown, ["X"], "a hidden name is not re-exported");
}

#[test]
fn dart_prefixed_types_generic_typedefs_and_local_functions() {
    let extracted = extract(
        "lib/prefixed.dart",
        r"
import 'package:x/x.dart' as pkg;
typedef int Compare<T>(T a, T b);

class Holder extends pkg. /* note */ Base {
  void build() {
    var a = new pkg.Widget();
    var b = const pkg.Style();
    void local() {}
  }
}

extension on String {
  void clean() { void hidden() {} }
}
",
    );
    symbol(&extracted, SymbolKind::TypeAlias, "Compare");
    let holder = symbol(&extracted, SymbolKind::Class, "Holder");
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&holder.id)
            && reference.kind == ReferenceKind::Extends
            && reference.name == "pkg.Base"
    }));
    let mut constructed = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Instantiates)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    constructed.sort_unstable();
    assert_eq!(constructed, ["pkg.Style", "pkg.Widget"]);
    for local in ["local", "hidden"] {
        let function = symbol(&extracted, SymbolKind::Function, local);
        assert_eq!(function.visibility, None, "{local} is local to its body");
        assert!(
            !function.export.exported,
            "{local} is not part of the library"
        );
    }
}

#[test]
fn dart_comments_inside_modifier_runs_keep_types_and_constness() {
    let extracted = extract(
        "lib/commented.dart",
        r"
const /* note */ answer = 42;
final /* note */ String label = name();

class Store {
  Database /* injected */ db;
  Map</* key */ String, int> counts;
}
",
    );
    let answer = symbol(&extracted, SymbolKind::Constant, "answer");
    assert_eq!(answer.qualified_name, "answer");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !(symbol.name == "answer" && symbol.kind == SymbolKind::Variable)),
        "a commented const stays a constant"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Variable, "label").kind,
        SymbolKind::Variable
    );
    let db = symbol(&extracted, SymbolKind::Field, "db");
    assert_eq!(db.signature.as_deref(), Some("Database db"));
    let counts = symbol(&extracted, SymbolKind::Field, "counts");
    assert_eq!(
        counts.signature, None,
        "a comment inside the type never enters a signature"
    );
    assert!(
        !format!("{:?}", extracted.symbols).contains("injected"),
        "comment text never reaches names or signatures"
    );
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

#[test]
fn escaped_dart_module_operands_abstain_before_import_facts() {
    for template in ["import '@VALUE@';\n", "export '@VALUE@';\n"] {
        escaped_specifiers::assert_escaped_specifiers_abstain("main.dart", template);
    }
}

#[test]
fn dart_imports_and_exports_screen_credential_bearing_uris() {
    credential_support::assert_screened(
        "main.dart",
        "import '@VALUE@';\nexport '@VALUE@';\n",
        "https://example.invalid/module",
    );
}
