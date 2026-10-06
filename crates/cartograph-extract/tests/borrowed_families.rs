//! Clojure, Common Lisp, Lean, `ReScript`, and Solidity extraction contracts.
//!
//! The scenarios are the v1 extractor acceptance sources, extended with the
//! negative cases each dedicated family must keep out of the graph.

mod credential_support;
mod dependency_ownership;
#[path = "credential_support/escaped_names.rs"]
mod escaped_names;

use cartograph_domain::{ReferenceKind, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
const SECRET_SENTINEL: &str = "sk_live_borrowed_family_secret";
/// Cancellation polls a full extraction of each fixture makes beyond parsing.
const MIN_WALK_POLLS: usize = 16;
/// Cancellation points sampled across one extraction.
const CANCELLATION_SAMPLES: usize = 48;
/// Qualified-type segments in `new A0.A1...()`, nesting past the walk's bound.
const DEEP_QUALIFIED_SEGMENTS: usize = 40;

const CLOJURE_SOURCE: &str = r#"
(ns demo.core
  (:require [clojure.string :as str]
            [demo.util :refer [helper]]))

(defonce default-name "world")

(defn greet
  "Greets a user."
  [name]
  (str/upper-case (helper name)))

(defn- hidden [x] (+ x 1))

(defmacro with-log [expr]
  (list 'do expr))
"#;

const COMMON_LISP_SOURCE: &str = r#"
(defpackage #:demo.core
  (:use #:cl)
  (:import-from #:demo.util #:helper))

(in-package #:demo.core)

(defparameter *default-name* "world")

(defun greet (name)
  (string-upcase (helper name)))

(defmacro with-log (expr)
  (list 'progn expr))

(defclass user ()
  ((name :initarg :name)))
"#;

const LEAN_SOURCE: &str = r"
import Mathlib.Data.Nat.Basic

structure User where
  name : String

inductive Role where
  | admin
  | user

def greet (u : User) : String := u.name
theorem id_eq (n : Nat) : n = n := rfl
abbrev UserName := String
";

const RESCRIPT_SOURCE: &str = r#"open Belt
include Js.Promise

type color = Red | Green
type person = {name: string, age: int}
type id = string
exception NotFound

@module("fs") external readFile: string => string = "readFileSync"

let add = (a: int, b: int): int => a + b
let x = 5
let run = async () => await fetch()
let use = () => x->add(1)->ignore

module type Shape = {
  let area: float => float
}

module Utils = {
  let helper = (y) => add(y, 1)
}

module Alias = Belt.Array
"#;

const SOLIDITY_SOURCE: &str = r#"
pragma solidity ^0.8.0;
import "./SafeMath.sol";

contract Vault {
  struct Entry { uint amount; }
  uint public total;

  function helper(uint amount) private returns (uint) {
    return amount * 2;
  }

  function deposit(uint amount) public returns (bool) {
    uint doubled = helper(amount);
    return doubled > 0;
  }
}
"#;

#[test]
fn clojure_extracts_namespace_imports_defs_privacy_signatures_and_list_head_calls() {
    let extracted = extract("src/demo/core.clj", CLOJURE_SOURCE);

    let namespace = symbol(&extracted, SymbolKind::Namespace, "demo.core");
    assert!(
        namespace.span.end_byte()
            < symbol(&extracted, SymbolKind::Constant, "default-name")
                .span
                .start_byte(),
        "the namespace must span only its ns form, not the whole file"
    );
    symbol(&extracted, SymbolKind::Import, "clojure.string");
    symbol(&extracted, SymbolKind::Import, "demo.util");
    assert_eq!(
        references_of(&extracted, ReferenceKind::Imports),
        ["clojure.string", "demo.util"]
    );

    let constant = symbol(&extracted, SymbolKind::Constant, "default-name");
    assert!(constant.export.exported);
    let greet = symbol(&extracted, SymbolKind::Function, "greet");
    assert_eq!(greet.qualified_name, "greet");
    assert_eq!(greet.signature.as_deref(), Some("[name]"));
    assert_eq!(greet.visibility, Some(Visibility::Public));
    assert!(greet.export.exported);
    let hidden = symbol(&extracted, SymbolKind::Function, "hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert!(!hidden.export.exported);
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "with-log")
            .signature
            .as_deref(),
        Some("[expr]")
    );

    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "str/upper-case", Some(greet))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "helper", Some(greet))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "+", Some(hidden))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(
            ReferenceKind::Calls,
            "list",
            Some(symbol(&extracted, SymbolKind::Function, "with-log"))
        )
    ));
    for head in [
        "ns", "defn", "defn-", "defonce", "defmacro", ":require", "do",
    ] {
        assert!(
            !has_any_reference(&extracted, ReferenceKind::Calls, head),
            "{head} is not a call"
        );
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Module),
        "the file must not collapse into a module: {:?}",
        names(&extracted)
    );
    assert_no_wildcard_bindings(&extracted);
    assert_eq!(
        named_bindings(&extracted),
        Vec::<(&str, &str)>::new(),
        "an imported name must not be bound to a module no resolver can map, \
         or the call to it can never resolve to its project definition"
    );
}

#[test]
fn clojure_metadata_privacy_punctuated_names_arities_and_special_forms() {
    let source = format!(
        r#"(defn ^:private ready? [] (foo 1) (:key {{:key 1}}))
(def ^{{:private true}} secret-key "{SECRET_SENTINEL}")
(defn multi ([a] a) ([a b] (+ a b)))
(defn guarded [x] (if (pos? x) (do (recur (dec x))) (let [y x] (fn [] y))))
(defn leaky [{{:keys [a] :or {{a "{SECRET_SENTINEL}"}}}}] a)
((comp first rest) [1 2])
"#
    );
    let extracted = extract("src/app/meta.clj", &source);

    let ready = symbol(&extracted, SymbolKind::Function, "ready?");
    assert_eq!(ready.visibility, Some(Visibility::Private));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "foo", Some(ready))
    ));
    let secret = symbol(&extracted, SymbolKind::Constant, "secret-key");
    assert_eq!(secret.visibility, Some(Visibility::Private));
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "multi")
            .signature
            .as_deref(),
        Some("[a] [a b]")
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "leaky").signature,
        None,
        "a literal-bearing parameter vector is not a signature"
    );
    for call in ["pos?", "dec"] {
        assert!(
            has_any_reference(&extracted, ReferenceKind::Calls, call),
            "{call}"
        );
    }
    for not_call in [
        "if",
        "do",
        "recur",
        "let",
        "fn",
        ":key",
        "(comp first rest)",
    ] {
        assert!(
            !has_any_reference(&extracted, ReferenceKind::Calls, not_call),
            "{not_call} must not be a call"
        );
    }
    assert!(
        has_any_reference(&extracted, ReferenceKind::Calls, "comp"),
        "the inner (comp first rest) list is still a call to comp"
    );
    assert_no_secret(&extracted);
}

#[test]
fn common_lisp_extracts_packages_imports_constants_functions_classes_and_calls() {
    let extracted = extract("src/demo/core.lisp", COMMON_LISP_SOURCE);

    let namespaces: Vec<_> = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Namespace)
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert_eq!(
        namespaces,
        ["demo.core"],
        "in-package re-enters the package"
    );
    symbol(&extracted, SymbolKind::Import, "cl");
    symbol(&extracted, SymbolKind::Import, "demo.util");
    assert_eq!(
        references_of(&extracted, ReferenceKind::Imports),
        ["cl", "demo.util"],
        "import-from imports only its package, never the imported symbols"
    );
    symbol(&extracted, SymbolKind::Constant, "*default-name*");
    let greet = symbol(&extracted, SymbolKind::Function, "greet");
    assert_eq!(greet.signature.as_deref(), Some("(name)"));
    let with_log = symbol(&extracted, SymbolKind::Function, "with-log");
    assert_eq!(with_log.signature.as_deref(), Some("(expr)"));
    symbol(&extracted, SymbolKind::Class, "user");

    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "string-upcase", Some(greet))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "helper", Some(greet))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "list", Some(with_log))
    ));
    for head in [
        "defpackage",
        "defparameter",
        "in-package",
        "use",
        "import-from",
        "defclass",
        "name",
    ] {
        assert!(
            !has_any_reference(&extracted, ReferenceKind::Calls, head),
            "{head} is not a call"
        );
    }
    assert_no_wildcard_bindings(&extracted);
    assert_eq!(
        named_bindings(&extracted),
        Vec::<(&str, &str)>::new(),
        "an imported name must not be bound to a module no resolver can map, \
         or the call to it can never resolve to its project definition"
    );
}

#[test]
fn common_lisp_kinds_designators_and_binding_forms_are_not_misread_as_calls() {
    let source = r#"(defvar *count* 0)
(defconstant +max+ 10)
(define-condition my-error (error) ())
(defstruct (point (:conc-name p-)) x y)
(require "asdf")
(use-package :alexandria)
(defun opt (a &optional (b 10)) a)
(defun walk (items)
  (let ((y (seed)))
    (dolist (item items) (process item y)))
  (handler-case (risky) (error (e) (recover e)))
  (mapcar (lambda (v w) (combine v w)) items)
  (cl:format t "done"))
"#;
    let extracted = extract("src/walk.lisp", source);

    symbol(&extracted, SymbolKind::Constant, "*count*");
    symbol(&extracted, SymbolKind::Constant, "+max+");
    symbol(&extracted, SymbolKind::Class, "my-error");
    symbol(&extracted, SymbolKind::Struct, "point");
    symbol(&extracted, SymbolKind::Import, "asdf");
    symbol(&extracted, SymbolKind::Import, "alexandria");
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "opt").signature,
        None,
        "a literal default keeps the lambda list out of the signature"
    );
    let walk = symbol(&extracted, SymbolKind::Function, "walk");
    for call in [
        "seed",
        "process",
        "risky",
        "recover",
        "mapcar",
        "combine",
        "cl:format",
    ] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Calls, call, Some(walk))
            ),
            "{call}"
        );
    }
    for not_call in [
        "y",
        "item",
        "error",
        "e",
        "v",
        "let",
        "dolist",
        "lambda",
        "handler-case",
        "p-",
        "point",
    ] {
        assert!(
            !has_any_reference(&extracted, ReferenceKind::Calls, not_call),
            "{not_call} must not be a call"
        );
    }
}

#[test]
fn lean_extracts_imports_structures_inductives_definitions_and_abbreviations() {
    let extracted = extract("Demo.lean", LEAN_SOURCE);

    symbol(&extracted, SymbolKind::Import, "Mathlib.Data.Nat.Basic");
    assert_eq!(
        references_of(&extracted, ReferenceKind::Imports),
        ["Mathlib.Data.Nat.Basic"]
    );
    symbol(&extracted, SymbolKind::Struct, "User");
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "name").qualified_name,
        "User::name"
    );
    symbol(&extracted, SymbolKind::Enum, "Role");
    assert_eq!(
        symbol(&extracted, SymbolKind::EnumMember, "admin").qualified_name,
        "Role::admin"
    );
    symbol(&extracted, SymbolKind::EnumMember, "user");
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "greet")
            .signature
            .as_deref(),
        Some("(u : User) : String")
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "id_eq").signature,
        None,
        "a proposition containing `=` is not a persisted signature"
    );
    symbol(&extracted, SymbolKind::TypeAlias, "UserName");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !matches!(symbol.kind, SymbolKind::Module | SymbolKind::Property)),
        "the import must not become a module owning the fields: {:?}",
        names(&extracted)
    );
    assert_no_wildcard_bindings(&extracted);
    assert_eq!(named_bindings(&extracted), Vec::<(&str, &str)>::new());
}

#[test]
fn lean_privacy_namespaces_and_class_inductives() {
    let source = "private def hidden : Nat := 1\nnamespace Geometry.Shapes\ndef area (w h : Nat) : Nat := w * h\nend Geometry.Shapes\nclass inductive Mode where\n  | fast\n";
    let extracted = extract("Shapes.lean", source);

    let hidden = symbol(&extracted, SymbolKind::Function, "hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert!(!hidden.export.exported);
    symbol(&extracted, SymbolKind::Namespace, "Geometry.Shapes");
    let area = symbol(&extracted, SymbolKind::Function, "area");
    assert_eq!(area.qualified_name, "Geometry.Shapes::area");
    assert!(area.export.exported);
    symbol(&extracted, SymbolKind::Enum, "Mode");
    symbol(&extracted, SymbolKind::EnumMember, "fast");
}

#[test]
fn rescript_extracts_opens_types_exceptions_and_externals() {
    let extracted = extract("src/Demo.res", RESCRIPT_SOURCE);

    assert_eq!(
        references_of(&extracted, ReferenceKind::Imports),
        ["Belt", "Js.Promise"],
        "include keeps its full module path"
    );
    symbol(&extracted, SymbolKind::Import, "Js.Promise");
    assert_no_wildcard_bindings(&extracted);
    assert_eq!(named_bindings(&extracted), Vec::<(&str, &str)>::new());

    symbol(&extracted, SymbolKind::Enum, "color");
    assert_eq!(
        symbol(&extracted, SymbolKind::EnumMember, "Red").qualified_name,
        "color::Red"
    );
    symbol(&extracted, SymbolKind::EnumMember, "Green");
    symbol(&extracted, SymbolKind::Struct, "person");
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "age").qualified_name,
        "person::age"
    );
    symbol(&extracted, SymbolKind::TypeAlias, "id");
    symbol(&extracted, SymbolKind::TypeAlias, "NotFound");
    let read_file = symbol(&extracted, SymbolKind::Function, "readFile");
    assert_eq!(
        read_file.signature, None,
        "`=>` types are not literal-free signatures"
    );
}

#[test]
fn rescript_extracts_callable_lets_pipes_and_modules() {
    let extracted = extract("src/Demo.res", RESCRIPT_SOURCE);

    let add = symbol(&extracted, SymbolKind::Function, "add");
    assert_eq!(add.signature.as_deref(), Some("(a: int, b: int): int"));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::TypeOf, "int", Some(add))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Returns, "int", Some(add))
    ));
    symbol(&extracted, SymbolKind::Variable, "x");
    assert!(
        symbol(&extracted, SymbolKind::Function, "run")
            .execution
            .async_symbol
    );
    let use_fn = symbol(&extracted, SymbolKind::Function, "use");
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "add", Some(use_fn))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "ignore", Some(use_fn))
    ));

    symbol(&extracted, SymbolKind::Interface, "Shape");
    symbol(&extracted, SymbolKind::Namespace, "Utils");
    let helper = symbol(&extracted, SymbolKind::Function, "helper");
    assert_eq!(helper.qualified_name, "Utils::helper");
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "add", Some(helper))
    ));
    let alias = symbol(&extracted, SymbolKind::Namespace, "Alias");
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::References, "Belt.Array", Some(alias))
    ));

    for parameter in ["a", "b", "y"] {
        assert!(
            extracted
                .symbols
                .iter()
                .all(|symbol| symbol.name != parameter),
            "parameter {parameter} must not become a declaration"
        );
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !(symbol.name == "add" && symbol.kind == SymbolKind::Variable))
    );
}

#[test]
fn rescript_typed_callable_bindings_record_types_and_owned_calls() {
    let source = format!(
        "let f = (x: payload): result => helper(x)\nexternal key: string = \"{SECRET_SENTINEL}\"\nlet _ = boot()\nlet (p, q) = (1, 2)\nmodule M = N\n"
    );
    let extracted = extract("src/Typed.res", &source);

    let f = symbol(&extracted, SymbolKind::Function, "f");
    assert_eq!(f.signature.as_deref(), Some("(x: payload): result"));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::TypeOf, "payload", Some(f))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Returns, "result", Some(f))
    ));
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "helper", Some(f))
    ));
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "key")
            .signature
            .as_deref(),
        Some(": string")
    );
    assert!(has_any_reference(&extracted, ReferenceKind::Calls, "boot"));
    assert!(extracted.symbols.iter().all(|symbol| symbol.name != "_"));
    let module = symbol(&extracted, SymbolKind::Namespace, "M");
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::References, "N", Some(module))
    ));
    assert_no_secret(&extracted);
}

#[test]
fn solidity_extracts_contract_members_signatures_visibility_and_imports() {
    let extracted = extract("contracts/Vault.sol", SOLIDITY_SOURCE);

    symbol(&extracted, SymbolKind::Class, "Vault");
    symbol(&extracted, SymbolKind::Struct, "Entry");
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "amount").qualified_name,
        "Vault::Entry::amount"
    );
    let total = symbol(&extracted, SymbolKind::Field, "total");
    assert_eq!(total.visibility, Some(Visibility::Public));
    assert!(total.export.exported);
    let helper = symbol(&extracted, SymbolKind::Method, "helper");
    assert_eq!(helper.visibility, Some(Visibility::Private));
    assert!(!helper.export.exported);
    let deposit = symbol(&extracted, SymbolKind::Method, "deposit");
    assert_eq!(
        deposit.signature.as_deref(),
        Some("(uint amount) returns (bool)")
    );
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "helper", Some(deposit))
    ));
    let member_calls: Vec<_> = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
        .map(|reference| {
            (
                reference.name.as_str(),
                reference.resolution_name.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        member_calls,
        [("helper", Some("Vault::helper"))],
        "a bare call to a contract callable resolves to that contract's member"
    );

    symbol(&extracted, SymbolKind::Import, "./SafeMath.sol");
    symbol(&extracted, SymbolKind::Import, "pragma solidity ^0.8.0");
    assert_eq!(
        references_of(&extracted, ReferenceKind::Imports),
        ["./SafeMath.sol"],
        "a pragma is not a module dependency"
    );
    assert!(
        extracted
            .import_bindings
            .iter()
            .any(|binding| binding.module_specifier == "./SafeMath.sol")
    );
    for spurious in ["bool", "uint"] {
        assert!(
            extracted
                .symbols
                .iter()
                .all(|symbol| symbol.name != spurious),
            "return type {spurious} must not become a declaration"
        );
    }
    assert!(
        extracted.symbols.iter().all(|symbol| !matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Variable
        ) || symbol.name == "doubled"),
        "contract callables are methods and state variables are fields: {:?}",
        names(&extracted)
    );
}

#[test]
fn solidity_modifiers_enum_values_special_callables_and_interfaces() {
    let source = r#"pragma solidity >=0.8.0;
contract Token is Base {
  enum State { Active, Closed }
  address internal owner;
  modifier onlyOwner() { require(msg.sender == owner); _; }
  constructor() Base("sk_live_borrowed_family_secret") {}
  receive() external payable { settle(); freeHelper(1); }
  function pay(address to) external onlyOwner returns (uint paid) { return 1; }
  function settle() internal {}
}
interface IToken { function pay(address to) external returns (uint); }
function freeHelper(uint a) pure returns (uint) { return a; }
"#;
    let extracted = extract("contracts/Token.sol", source);

    let modifier = symbol(&extracted, SymbolKind::Method, "onlyOwner");
    assert_eq!(modifier.qualified_name, "Token::onlyOwner");
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "require", Some(modifier))
    ));
    assert_eq!(
        symbol(&extracted, SymbolKind::EnumMember, "Closed").qualified_name,
        "Token::State::Closed"
    );
    let owner = symbol(&extracted, SymbolKind::Field, "owner");
    assert_eq!(owner.visibility, Some(Visibility::Internal));
    assert!(!owner.export.exported);
    symbol(&extracted, SymbolKind::Method, "constructor");
    let receive = symbol(&extracted, SymbolKind::Method, "receive");
    let resolution_names: Vec<_> = extracted
        .references
        .iter()
        .filter(|reference| reference.owner.as_ref() == Some(&receive.id))
        .map(|reference| {
            (
                reference.name.as_str(),
                reference.resolution_name.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        resolution_names,
        [("settle", Some("Token::settle")), ("freeHelper", None)],
        "a later contract callable is in scope; a free function is not a member"
    );
    let pay = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "Token::pay")
        .unwrap_or_else(|| panic!("missing Token::pay: {:?}", names(&extracted)));
    assert_eq!(pay.kind, SymbolKind::Method);
    assert_eq!(
        pay.visibility,
        Some(Visibility::Public),
        "external is public"
    );
    assert_eq!(
        pay.signature.as_deref(),
        Some("(address to) returns (uint paid)")
    );
    assert!(
        extracted
            .symbols
            .iter()
            .any(|symbol| symbol.qualified_name == "IToken::pay"
                && symbol.kind == SymbolKind::Method)
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "freeHelper").qualified_name,
        "freeHelper"
    );
    for spurious in ["Base", "paid"] {
        assert!(
            extracted
                .symbols
                .iter()
                .all(|symbol| symbol.name != spurious || symbol.kind == SymbolKind::Import),
            "{spurious} must not become a declaration: {:?}",
            names(&extracted)
        );
    }
    assert!(has_reference(
        &extracted,
        Expected::new(
            ReferenceKind::Inherits,
            "Base",
            Some(symbol(&extracted, SymbolKind::Class, "Token"))
        )
    ));
    assert_no_secret(&extracted);
}

#[test]
fn lisp_quoted_discarded_and_declarative_forms_never_read_as_calls() {
    let clojure = r"(defn run [x] (comment (ghost x)) '(quoted x) #_(discarded x) (real x))
(defn keyed [{:keys [a] :or {a :fallback}}] a)
(defprotocol Shape (area [this]))
(defrecord Circle [r] Shape (area [this] (scale r)))
";
    let extracted = extract("src/shapes.clj", clojure);
    assert!(has_any_reference(&extracted, ReferenceKind::Calls, "real"));
    assert!(has_any_reference(&extracted, ReferenceKind::Calls, "scale"));
    for ghost in ["ghost", "quoted", "discarded", "area", "Shape"] {
        assert!(
            !has_any_reference(&extracted, ReferenceKind::Calls, ghost),
            "{ghost} is data or a declaration, not a call"
        );
    }
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "keyed").signature,
        None,
        "a keyword default is a literal and keeps the vector out of the signature"
    );
    let protocol = symbol(&extracted, SymbolKind::Interface, "Shape");
    assert!(protocol.implementation.declaration_only);
    symbol(&extracted, SymbolKind::Class, "Circle");

    let common_lisp = r"(defun run () (quote (ghost)) '(quoted 1) (|escaped| 2))
(defun opt (a &optional (b t)) a)
(defmethod speak ((a animal)) (format t a))
";
    let extracted = extract("src/run.lisp", common_lisp);
    assert!(has_any_reference(
        &extracted,
        ReferenceKind::Calls,
        "escaped"
    ));
    for ghost in ["ghost", "quoted", "quote"] {
        assert!(
            !has_any_reference(&extracted, ReferenceKind::Calls, ghost),
            "{ghost} is quoted data, not a call"
        );
    }
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "opt").signature,
        None,
        "an optional default can be a literal"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "speak")
            .signature
            .as_deref(),
        Some("((a animal))"),
        "a method specializer is retained"
    );
}

#[test]
fn rescript_interface_files_and_module_signatures_are_walked() {
    let extracted = extract(
        "src/Api.resi",
        "type t\nlet make: string => t\nmodule Inner: {\n  let size: t => int\n}\n",
    );
    symbol(&extracted, SymbolKind::TypeAlias, "t");
    let make = symbol(&extracted, SymbolKind::Variable, "make");
    assert!(
        make.implementation.declaration_only,
        "a signature is not the implementation"
    );
    let size = symbol(&extracted, SymbolKind::Variable, "size");
    assert_eq!(size.qualified_name, "Inner::size");
    assert!(size.implementation.declaration_only);
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::TypeOf, "t", Some(make))
    ));

    let implementation = extract(
        "src/Api.res",
        "type t = string\nlet make = (s: string) => s\n",
    );
    assert!(
        !symbol(&implementation, SymbolKind::Function, "make")
            .implementation
            .declaration_only
    );
}

#[test]
fn rescript_qualified_types_and_member_calls_keep_their_paths() {
    let extracted = extract(
        "src/Paths.res",
        "type t = string\nmodule A = { type t = int }\nlet f = (x: A.t, cb) => cb.onClick(x)\n",
    );
    let f = symbol(&extracted, SymbolKind::Function, "f");
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::TypeOf, "A.t", Some(f))
    ));
    assert!(
        !has_reference(
            &extracted,
            Expected::new(ReferenceKind::TypeOf, "t", Some(f))
        ),
        "a qualified type must not degrade to the unrelated top-level `t`"
    );
    assert!(has_reference(
        &extracted,
        Expected::new(ReferenceKind::Calls, "cb.onClick", Some(f))
    ));
}

#[test]
fn clojure_templates_letfn_and_metadata_values_are_read_structurally() {
    let source = r#"(defmacro make [n] `(defn ~n [] (ghost ~(real n) ~@(more n))))
(defn ^{:doc ":private true"} documented [] 1)
(defn ^{:private true :doc "x"} hidden [] 1)
(defn use-local [] (letfn [(local [x] (helper x))] (local 1)))
(defn arities [] (letfn [(f ([x] (first-call x)) ([x y] (second-call x y)))] (f 1)))
(defmacro skip [] `(list #_~(dropped) ~(kept)))
"#;
    let extracted = extract("src/templates.clj", source);

    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "documented").visibility,
        Some(Visibility::Public),
        "privacy text inside a docstring is not privacy metadata"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "hidden").visibility,
        Some(Visibility::Private)
    );
    for call in [
        "real",
        "more",
        "helper",
        "first-call",
        "second-call",
        "f",
        "kept",
    ] {
        assert_eq!(call_count(&extracted, call), 1, "{call}");
    }
    assert_eq!(
        call_count(&extracted, "dropped"),
        0,
        "a discarded unquote is never read"
    );
    assert_eq!(
        call_count(&extracted, "local"),
        1,
        "only the invocation calls a letfn binding, not its definition"
    );
    assert_eq!(
        call_count(&extracted, "ghost"),
        0,
        "template data is not a call"
    );
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Function)
            .count(),
        6,
        "a syntax-quoted defn is generated code, not a declaration: {:?}",
        names(&extracted)
    );
}

#[test]
fn common_lisp_literal_names_uppercase_defaults_escapes_and_templates() {
    let source = format!(
        r#"(defun "{SECRET_SENTINEL}" () nil)
(defun opt (a &OPTIONAL (b T)) a)
(defun mixed (a &Key (b T)) a)
(defpackage :edge (:import-from "LIB" "HELPER" #:other))
(defun |Weird Name| () (|Weird Name|))
(defun run () (cond (t (real)) ((ready) (go-on))))
(defmacro make (n) `(defun ,n () (ghost ,(real-call n) ,@(more n))))
"#
    );
    let extracted = extract("src/edge.lisp", &source);

    assert_no_secret(&extracted);
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "opt").signature,
        None,
        "lambda-list keywords are case-insensitive"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "mixed").signature,
        None
    );
    assert_eq!(
        references_of(&extracted, ReferenceKind::Imports),
        ["LIB"],
        "a string package designator is imported, its symbols are not"
    );
    assert_eq!(named_bindings(&extracted), Vec::<(&str, &str)>::new());
    symbol(&extracted, SymbolKind::Function, "Weird Name");
    assert_eq!(call_count(&extracted, "Weird Name"), 1);
    for call in ["real", "ready", "go-on", "real-call", "more"] {
        assert_eq!(call_count(&extracted, call), 1, "{call}");
    }
    for not_call in ["t", "ghost"] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Function)
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["opt", "mixed", "Weird Name", "run", "make"],
        "a string is never a function name and templates declare nothing"
    );
}

#[test]
fn solidity_parameters_shadow_contract_members() {
    let source = r"contract Vault {
  function helper(uint a) internal pure returns (uint) { return a; }
  function apply(function(uint) internal pure returns (uint) helper, uint a) internal pure returns (uint) { return helper(a); }
  function direct(uint a) internal pure returns (uint) { return helper(a); }
  function apply(uint a) internal pure returns (uint) { return helper(a); }
}
";
    let extracted = extract("contracts/Shadow.sol", source);
    let resolutions: Vec<_> = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name != "helper")
        .map(|owner| {
            let resolution = extracted
                .references
                .iter()
                .find(|reference| {
                    reference.kind == ReferenceKind::Calls
                        && reference.owner.as_ref() == Some(&owner.id)
                })
                .and_then(|reference| reference.resolution_name.as_deref());
            (owner.name.as_str(), resolution)
        })
        .collect();
    assert_eq!(
        resolutions,
        [
            ("apply", None),
            ("direct", Some("Vault::helper")),
            ("apply", Some("Vault::helper")),
        ],
        "a parameter shadows the member only in the overload that declares it"
    );
}

#[test]
fn solidity_libraries_interfaces_and_modifier_invocations() {
    let source = r"library Math {
  function twice(uint a) internal pure returns (uint) { return half(a) * 4; }
  function half(uint a) internal pure returns (uint) { return a / 2; }
}
interface IVault { function deposit(uint amount) external returns (bool); }
contract Vault is Base {
  modifier guarded() { _; }
  constructor() Base(1) {}
  function deposit(uint amount) external guarded inherited returns (bool) { return true; }
}
";
    let extracted = extract("contracts/Math.sol", source);

    symbol(&extracted, SymbolKind::Class, "Math");
    let twice = symbol(&extracted, SymbolKind::Method, "twice");
    assert_eq!(twice.qualified_name, "Math::twice");
    let calls: Vec<_> = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
        .map(|reference| {
            (
                reference.name.as_str(),
                reference.resolution_name.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("half", Some("Math::half")),
            ("guarded", Some("Vault::guarded")),
            ("inherited", None),
        ],
        "library members resolve in-library; header modifiers are calls; base \
         constructor arguments are not"
    );
    let declared = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "IVault::deposit")
        .unwrap_or_else(|| panic!("missing IVault::deposit: {:?}", names(&extracted)));
    assert!(declared.implementation.declaration_only);
    let implemented = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "Vault::deposit")
        .unwrap_or_else(|| panic!("missing Vault::deposit: {:?}", names(&extracted)));
    assert!(!implemented.implementation.declaration_only);
}

#[test]
fn dedicated_families_are_deterministic_and_fail_closed_on_cancellation() {
    for (path, source) in [
        ("src/demo/core.clj", CLOJURE_SOURCE),
        ("src/demo/core.lisp", COMMON_LISP_SOURCE),
        ("Demo.lean", LEAN_SOURCE),
        ("contracts/Vault.sol", SOLIDITY_SOURCE),
        (
            "src/Det.res",
            "open Belt\nlet add = (a, b) => a + b\nmodule M = { let x = add(1, 2) }\n",
        ),
    ] {
        assert_eq!(extract(path, source), extract(path, source), "{path}");
        let snapshot = snapshot(path, source);
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
        let mut polls = 0_usize;
        extractor
            .extract_with_cancellation(&snapshot, || {
                polls = polls.saturating_add(1);
                false
            })
            .unwrap_or_else(|error| panic!("uncancelled extraction failed for {path}: {error}"));
        assert!(
            polls > MIN_WALK_POLLS,
            "{path} must poll cancellation while walking"
        );
        // Cancel at points spread over the whole run, including mid-walk
        // inside the family visitors, not only before parsing.
        let stride = polls.div_ceil(CANCELLATION_SAMPLES).max(1);
        for cancel_at in (1..=polls).step_by(stride).chain([polls]) {
            let mut seen = 0_usize;
            let outcome = extractor.extract_with_cancellation(&snapshot, || {
                seen = seen.saturating_add(1);
                seen >= cancel_at
            });
            assert!(
                matches!(outcome, Err(ExtractError::Cancelled)),
                "{path} must fail closed when cancelled at poll {cancel_at} of {polls}"
            );
        }
    }
}

#[test]
fn common_lisp_definitions_are_recognized_in_any_letter_case() {
    let source = r"(DEFUN GREET (NAME) (STRING-UPCASE NAME))
(Defun Mixed (a) (Helper a))
(DEFMETHOD SPEAK :BEFORE ((A ANIMAL)) (NOTIFY A))
(CL:DEFMACRO WITH-LOG (EXPR) (LIST EXPR))
(DEFUN (SETF SLOT) (V X) (STORE V X))
(DEFMETHOD SCORE + ((S INTEGER)) (TALLY S))
(COMMON-LISP:DEFUN QUALIFIED (Q) (CHECK Q))
(CL::DEFUN DOUBLE (D) (CHECK D))
(DEFMETHOD EMPTY NIL (FILL-IN))
";
    let extracted = extract("src/upper.lisp", source);

    let greet = symbol(&extracted, SymbolKind::Function, "GREET");
    assert_eq!(greet.signature.as_deref(), Some("(NAME)"));
    assert!(greet.export.exported);
    let mixed = symbol(&extracted, SymbolKind::Function, "Mixed");
    assert_eq!(mixed.signature.as_deref(), Some("(a)"));
    let speak = symbol(&extracted, SymbolKind::Function, "SPEAK");
    assert_eq!(
        speak.signature.as_deref(),
        Some("((A ANIMAL))"),
        "a method qualifier is not the lambda list"
    );
    symbol(&extracted, SymbolKind::Function, "WITH-LOG");
    let score = symbol(&extracted, SymbolKind::Function, "SCORE");
    assert_eq!(
        score.signature.as_deref(),
        Some("((S INTEGER))"),
        "a non-keyword method qualifier is skipped, not read as the lambda list"
    );
    let qualified = symbol(&extracted, SymbolKind::Function, "QUALIFIED");
    assert_eq!(qualified.signature.as_deref(), Some("(Q)"));
    symbol(&extracted, SymbolKind::Function, "DOUBLE");
    let empty = symbol(&extracted, SymbolKind::Function, "EMPTY");
    assert_eq!(empty.signature, None, "NIL is the empty lambda list");
    for (call, owner) in [
        ("FILL-IN", empty),
        ("STRING-UPCASE", greet),
        ("Helper", mixed),
        ("NOTIFY", speak),
        ("TALLY", score),
        ("CHECK", qualified),
    ] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Calls, call, Some(owner))
            ),
            "{call} is called by its definition"
        );
    }
    assert_eq!(
        call_count(&extracted, "STORE"),
        1,
        "an unnamed definition still walks its body"
    );
    for not_call in [
        "DEFUN",
        "Defun",
        "DEFMETHOD",
        "CL:DEFMACRO",
        "NAME",
        "a",
        "A",
        "V",
        "SETF",
        ":BEFORE",
        "S",
        "Q",
        "CL::DEFUN",
        "COMMON-LISP:DEFUN",
    ] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
    assert_eq!(function_count(&extracted), 8, "{:?}", names(&extracted));
}

#[test]
fn common_lisp_escaped_heads_keep_their_exact_spelling() {
    let source = r"(defun |defun| (x y) (+ x y))
(defun shadow-op (x y) (|defun| x y))
(defun upper-escape (u) (|DEFUN| esc (u) (inner u)))
(defun |COMMON-LISP:DEFUN| (x) x)
(defun literal-colon (x) (|COMMON-LISP:DEFUN| x))
";
    let extracted = extract("src/escaped.lisp", source);

    symbol(&extracted, SymbolKind::Function, "defun");
    let shadow_op = symbol(&extracted, SymbolKind::Function, "shadow-op");
    assert!(
        has_reference(
            &extracted,
            Expected::new(ReferenceKind::Calls, "defun", Some(shadow_op))
        ),
        "an escaped lower-case |defun| is an ordinary function"
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "esc").qualified_name,
        "upper-escape::esc",
        "an escaped upper-case |DEFUN| is the standard operator"
    );
    let literal_colon = symbol(&extracted, SymbolKind::Function, "literal-colon");
    assert!(
        has_reference(
            &extracted,
            Expected::new(
                ReferenceKind::Calls,
                "COMMON-LISP:DEFUN",
                Some(literal_colon)
            )
        ),
        "an escaped colon is part of the name, not a package separator"
    );
    assert_eq!(
        call_count(&extracted, "COMMON-LISP:DEFUN"),
        1,
        "only the escaped call is a call"
    );
    for not_call in ["x", "u", "DEFUN", "|DEFUN|", "esc"] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
    assert_eq!(function_count(&extracted), 6, "{:?}", names(&extracted));
}

#[test]
fn common_lisp_packages_fold_case_and_declarations_are_not_calls() {
    let source = r#"(defpackage :app (:use :cl))
(in-package "APP")
(defpackage "lower")
(defpackage "LOWER")
(in-package #:lower)
(declaim (inline fast))
(defun fast (x) (declare (ignore x) (optimize speed)) (work))
"#;
    let extracted = extract("src/app.lisp", source);

    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Namespace)
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["app", "lower", "LOWER"],
        "symbol designators are read in upper case; string designators keep their case"
    );
    assert_eq!(call_count(&extracted, "work"), 1);
    for not_call in ["inline", "ignore", "optimize", "declare", "declaim"] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
}

#[test]
fn clojure_anonymous_functions_and_metadata_prefixed_lists_call_their_heads() {
    let source = r"(defn %helper [x] x)
(defn run [m xs]
  (map #(helper %) xs)
  (-> 1 #(inc %))
  #(% 1)
  ^String (get m :k)
  #^String (fetch m)
  ^{:x (ghost)} (real)
  #(%1 %2 %&)
  (%helper xs)
  (% xs)
  ^{:x (hinted)} m
  (outer ^{:y (phantom)} [1]))
(defn outer-fn [] (defn inner-fn [] 1))
";
    let extracted = extract("src/anon.clj", source);

    let run = symbol(&extracted, SymbolKind::Function, "run");
    // Metadata on a collection literal is evaluated, so `phantom` is a call;
    // metadata on an invocation (`ghost`) is not.
    for call in [
        "map", "helper", "->", "inc", "get", "fetch", "real", "outer", "phantom",
    ] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Calls, call, Some(run))
            ),
            "{call}"
        );
    }
    assert!(
        has_reference(
            &extracted,
            Expected::new(ReferenceKind::Calls, "%helper", Some(run))
        ),
        "only the implicit arguments %, %N, and %& are not calls"
    );
    assert_eq!(
        call_count(&extracted, "%"),
        1,
        "% is an implicit argument only inside #(...); elsewhere it is a symbol"
    );
    for not_call in ["ghost", "hinted", "String", "%1", "%&", ":x"] {
        assert_eq!(
            call_count(&extracted, not_call),
            0,
            "{not_call} is metadata or an anonymous argument, not a call"
        );
    }
    let inner = symbol(&extracted, SymbolKind::Function, "inner-fn");
    assert!(
        !inner.export.exported,
        "a definition nested in a function is not a namespace export"
    );
    assert!(
        symbol(&extracted, SymbolKind::Function, "outer-fn")
            .export
            .exported
    );
}

#[test]
fn lean_grouped_field_binders_declare_every_name_but_constructor_parameters_do_not() {
    let source = "structure P where\n  (x y : Nat)\n  {u v : Nat}\n  z : Nat\ninductive Tree where\n  | leaf : Tree\n  | node (l r : Tree) : Tree\n";
    let extracted = extract("Grouped.lean", source);

    for field in ["x", "y", "u", "v", "z"] {
        assert_eq!(
            symbol(&extracted, SymbolKind::Field, field).qualified_name,
            format!("P::{field}")
        );
    }
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::EnumMember)
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["leaf", "node"],
        "constructor parameters are not members"
    );
}

#[test]
fn rescript_parenthesized_module_alias_references_its_target() {
    let extracted = extract(
        "src/Alias.res",
        "module Alias = (Belt.Array)\nmodule Plain = Belt.List\nmodule Noted = (/* note */ Belt.Map)\n",
    );
    for (module, target) in [
        ("Alias", "Belt.Array"),
        ("Plain", "Belt.List"),
        ("Noted", "Belt.Map"),
    ] {
        let owner = symbol(&extracted, SymbolKind::Namespace, module);
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::References, target, Some(owner))
            ),
            "{module} aliases {target}"
        );
    }
}

#[test]
fn solidity_named_return_parameters_shadow_contract_members() {
    let source = r"contract Vault {
  function helper() internal pure returns (uint) { return 1; }
  function pick() internal pure returns (function() internal pure returns (uint) helper) { helper(); }
  function direct() internal pure returns (uint) { return helper(); }
}
";
    let extracted = extract("contracts/Returns.sol", source);
    let resolution = |owner: &str| {
        let owner = symbol(&extracted, SymbolKind::Method, owner);
        extracted
            .references
            .iter()
            .find(|reference| {
                reference.kind == ReferenceKind::Calls
                    && reference.name == "helper"
                    && reference.owner.as_ref() == Some(&owner.id)
            })
            .map(|reference| reference.resolution_name.as_deref())
    };
    assert_eq!(
        resolution("pick"),
        Some(None),
        "a named return shadows the member"
    );
    assert_eq!(resolution("direct"), Some(Some("Vault::helper")));
}

#[test]
fn solidity_new_expressions_instantiate_their_contract_type() {
    // v1 tree-sitter.ts routes every `new_expression` to an `instantiates`
    // reference named by its type; the `new` keyword is never a callee.
    let source = r"contract Vault {
  function spawn() external returns (address) {
    Token fresh = new Token();
    Box boxed = new Box{value: 1}(2);
    lib.Pool pool = new lib.Pool();
    uint[] memory sizes = new uint[](3);
    return address(fresh);
  }
}
";
    let extracted = extract("contracts/Vault.sol", source);
    let spawn = symbol(&extracted, SymbolKind::Method, "spawn");
    for name in ["Token", "Box", "lib.Pool"] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Instantiates, name, Some(spawn))
            ),
            "missing instantiation of {name}: {:?}",
            extracted.references
        );
    }
    assert_eq!(
        references_of(&extracted, ReferenceKind::Instantiates).len(),
        3,
        "a primitive array allocation instantiates no contract type"
    );
    assert_eq!(call_count(&extracted, "new"), 0, "`new` is not a callee");

    let split = extract(
        "contracts/Split.sol",
        "contract F {\n  function make() external {\n    Token t = new\n      Token();\n  }\n}\n",
    );
    let token = split
        .references
        .iter()
        .find(|reference| reference.kind == ReferenceKind::Instantiates)
        .unwrap_or_else(|| panic!("missing instantiation: {:?}", split.references));
    assert_eq!(
        token.span.start_line(),
        3,
        "an instantiation sits on its `new` line"
    );

    let path = (0..DEEP_QUALIFIED_SEGMENTS)
        .map(|index| format!("A{index}"))
        .collect::<Vec<_>>()
        .join(".");
    let deep = extract(
        "contracts/Deep.sol",
        &format!("contract D {{\n  function make() external {{\n    new {path}();\n  }}\n}}\n"),
    );
    assert!(
        deep.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Instantiates
                && reference.name != "new"),
        "a construction past the depth bound abstains: {:?}",
        deep.references
    );
}

#[test]
fn solidity_calls_chained_on_a_construction_name_their_member() {
    // A call chained on a fresh construction calls the member it names; its
    // receiver has no name, and the `new` keyword is still no callee.
    let chained = extract(
        "contracts/Chain.sol",
        "contract C {\n  function run() external {\n    new Token(1).mint();\n    (new Token()).owner();\n    new Box{value: 1}().fill();\n    new lib.Pool().drain();\n  }\n}\n",
    );
    let run = symbol(&chained, SymbolKind::Method, "run");
    for (method, line) in [("mint", 3), ("owner", 4), ("fill", 5), ("drain", 6)] {
        let call = chained
            .references
            .iter()
            .find(|reference| reference.kind == ReferenceKind::Calls && reference.name == method)
            .unwrap_or_else(|| panic!("missing call of {method}: {:?}", chained.references));
        assert_eq!(
            call.owner.as_ref(),
            Some(&run.id),
            "{method} is called by run"
        );
        assert_eq!(
            call.span.start_line(),
            line,
            "{method} sits on its own line"
        );
    }
    assert_eq!(
        references_of(&chained, ReferenceKind::Calls).len(),
        4,
        "a construction chained into a call adds no other callee: {:?}",
        chained.references
    );
    for name in ["Token", "Box", "lib.Pool"] {
        assert!(
            has_any_reference(&chained, ReferenceKind::Instantiates, name),
            "a chained construction still instantiates {name}: {:?}",
            chained.references
        );
    }

    // The construction walk is bounded, but a long member chain that never
    // constructs anything keeps its call.
    let receiver = (0..DEEP_QUALIFIED_SEGMENTS)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>()
        .join(".");
    let long = extract(
        "contracts/Long.sol",
        &format!("contract L {{\n  function run() external {{\n    {receiver}.go();\n  }}\n}}\n"),
    );
    assert_eq!(
        references_of(&long, ReferenceKind::Calls),
        vec![format!("{receiver}.go").as_str()],
        "a deep non-construction member call keeps its callee"
    );
}

#[test]
fn common_lisp_defaults_run_but_type_specifiers_and_inner_templates_do_not() {
    let source = r"(defun run (a &optional (x (seed)) &key ((:k kk) (other)) &aux (z (third)))
  (the (unsigned-byte 8) (helper a x kk z)))
(DEFUN UP (A &OPTIONAL (B (SEED2) B-P) &REST R &KEY C) (WORK A B R C))
(defun plain ((s stream) n) (emit s n))
(defmacro make (x) `(outer `(inner ,(ghost) ,,x) ,(real) ,@(splice) #_,(gone)))
";
    let extracted = extract("src/defaults.lisp", source);

    let run = symbol(&extracted, SymbolKind::Function, "run");
    let up = symbol(&extracted, SymbolKind::Function, "UP");
    let plain = symbol(&extracted, SymbolKind::Function, "plain");
    let make = symbol(&extracted, SymbolKind::Function, "make");
    for (call, owner) in [
        ("seed", run),
        ("other", run),
        ("third", run),
        ("helper", run),
        ("SEED2", up),
        ("WORK", up),
        ("emit", plain),
        ("real", make),
        ("splice", make),
    ] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Calls, call, Some(owner))
            ),
            "{call} is called by its definition"
        );
        assert_eq!(call_count(&extracted, call), 1, "{call}");
    }
    // A type specifier, a defaulted variable or keyword pair, a supplied-p
    // variable, a specializer, an inner template's own unquote, and a
    // discarded form are never calls.
    for not_call in [
        "unsigned-byte",
        "the",
        "x",
        ":k",
        "B",
        "B-P",
        "s",
        "stream",
        "ghost",
        "inner",
        "outer",
        "gone",
    ] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
    assert_eq!(
        up.signature, None,
        "a defaulted lambda list is not retained"
    );
}

#[test]
fn clojure_core_qualified_operators_and_nested_templates_are_read_structurally() {
    let source = r"(clojure.core/defn run [x] (helper x))
(clojure.core/defn- hidden [y] (clojure.core/map inc y))
(defmacro m [x] `(do `(inner ~(ghost)) ~(real) ~@(splice) (f ~x)))
";
    let extracted = extract("src/core.clj", source);

    let run = symbol(&extracted, SymbolKind::Function, "run");
    assert_eq!(run.signature.as_deref(), Some("[x]"));
    assert!(run.export.exported);
    let hidden = symbol(&extracted, SymbolKind::Function, "hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert!(!hidden.export.exported);
    let m = symbol(&extracted, SymbolKind::Function, "m");
    for (call, owner) in [
        ("helper", run),
        ("clojure.core/map", hidden),
        ("real", m),
        ("splice", m),
    ] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Calls, call, Some(owner))
            ),
            "{call} is called by its definition"
        );
    }
    for not_call in [
        "clojure.core/defn",
        "clojure.core/defn-",
        "x",
        "ghost",
        "inner",
        "f",
    ] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
}

#[test]
fn rescript_parameter_defaults_and_parenthesized_callees_are_calls() {
    let extracted = extract(
        "src/Defaults.res",
        "let run = (~x=seed(), ~y: int=other(2), z) => (helper)(x + y + z)\n",
    );

    let run = symbol(&extracted, SymbolKind::Function, "run");
    for call in ["seed", "other", "helper"] {
        assert!(
            has_reference(
                &extracted,
                Expected::new(ReferenceKind::Calls, call, Some(run))
            ),
            "{call} is called by run"
        );
        assert_eq!(call_count(&extracted, call), 1, "{call}");
    }
    for not_call in ["x", "y", "z", "(helper)"] {
        assert_eq!(call_count(&extracted, not_call), 0, "{not_call}");
    }
    assert_eq!(
        run.signature, None,
        "a signature with default values is not retained"
    );
}

#[test]
fn rescript_computed_member_callees_abstain_and_keep_inner_calls() {
    let extracted = extract(
        "src/Computed.res",
        "let f = () => {get(/*sk_live_FAKE1234567890abcdef*/1).run(); get(1).run(); callbacks.onClick(); callbacks.nested.run(); (helper)(1)}\n",
    );
    let f = symbol(&extracted, SymbolKind::Function, "f");
    let calls = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Calls && reference.owner.as_ref() == Some(&f.id)
        })
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            "get",
            "get",
            "callbacks.onClick",
            "callbacks.nested.run",
            "helper"
        ]
    );
}

#[test]
fn rescript_module_qualified_record_members_keep_their_full_path() {
    let extracted = extract(
        "src/QualifiedMembers.res",
        "let f = () => {callbacks.Handler.run(); callbacks.Outer.Handler.run(); get(1).Handler.run()}\n",
    );
    let f = symbol(&extracted, SymbolKind::Function, "f");
    let calls = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Calls && reference.owner.as_ref() == Some(&f.id)
        })
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            "callbacks.Handler.run",
            "callbacks.Outer.Handler.run",
            "get"
        ]
    );
}

#[test]
fn rescript_module_member_scans_charge_extra_children() {
    let source = format!(
        "let f = () => callbacks.{}Handler.run()\n",
        "/* member comment */".repeat(64)
    );
    let extracted = extract("src/MemberComments.res", &source);
    assert!(
        !extracted
            .references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::Calls),
        "member recovery beyond the direct-child budget must abstain"
    );
}

#[test]
fn rescript_wide_binding_groups_keep_individual_spans() {
    for (keyword, binding, kind) in [
        ("type", "t", SymbolKind::TypeAlias),
        ("let rec", "f", SymbolKind::Function),
    ] {
        let source = format!(
            "{keyword} {}\n",
            (0..192)
                .map(|index| {
                    let value = if kind == SymbolKind::Function {
                        "() => ()"
                    } else {
                        "int"
                    };
                    format!("{binding}{index} = {value}")
                })
                .collect::<Vec<_>>()
                .join(" and ")
        );
        let extracted = extract("src/Wide.res", &source);
        for index in 0..192 {
            let declared = symbol(&extracted, kind, &format!("{binding}{index}"));
            assert!(declared.span.end_byte() - declared.span.start_byte() < 32);
        }
    }
}

fn snapshot(path: &str, source: &str) -> SourceSnapshot {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"))
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = snapshot(path, source);
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    let extracted = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"));
    assert!(
        extracted.diagnostics.is_empty(),
        "{path} must parse cleanly: {:?}",
        extracted.diagnostics
    );
    extracted
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
        .unwrap_or_else(|| panic!("missing {kind:?} {name}; extracted: {:?}", names(extracted)))
}

fn names(extracted: &ExtractedFile) -> Vec<String> {
    extracted
        .symbols
        .iter()
        .map(|symbol| format!("{:?} {}", symbol.kind, symbol.qualified_name))
        .collect()
}

fn references_of(extracted: &ExtractedFile, kind: ReferenceKind) -> Vec<&str> {
    extracted
        .references
        .iter()
        .filter(|reference| reference.kind == kind)
        .map(|reference| reference.name.as_str())
        .collect()
}

/// Number of extracted Function symbols.
fn function_count(extracted: &ExtractedFile) -> usize {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function)
        .count()
}

fn call_count(extracted: &ExtractedFile, name: &str) -> usize {
    extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls && reference.name == name)
        .count()
}

fn has_any_reference(extracted: &ExtractedFile, kind: ReferenceKind, name: &str) -> bool {
    extracted
        .references
        .iter()
        .any(|reference| reference.kind == kind && reference.name == name)
}

/// One reference the extraction must contain, with its exact owner.
#[derive(Clone, Copy)]
struct Expected<'name, 'file> {
    kind: ReferenceKind,
    name: &'name str,
    owner: Option<&'file ExtractedSymbol>,
}

impl<'name, 'file> Expected<'name, 'file> {
    const fn new(
        kind: ReferenceKind,
        name: &'name str,
        owner: Option<&'file ExtractedSymbol>,
    ) -> Self {
        Self { kind, name, owner }
    }
}

fn has_reference(extracted: &ExtractedFile, expected: Expected<'_, '_>) -> bool {
    extracted.references.iter().any(|reference| {
        reference.kind == expected.kind
            && reference.name == expected.name
            && reference.owner.as_ref() == expected.owner.map(|symbol| &symbol.id)
    })
}

/// A wildcard binding matches every reference name and would turn off the
/// project-wide fallback for the whole file, so none may be emitted; explicit
/// named bindings are allowed.
fn assert_no_wildcard_bindings(extracted: &ExtractedFile) {
    assert!(
        extracted
            .import_bindings
            .iter()
            .all(|binding| binding.local_name != "*" && binding.imported_name != "*"),
        "imports must not suppress project-wide call resolution: {:?}",
        extracted.import_bindings
    );
}

/// The explicitly bound names, as `(module, name)` pairs.
fn named_bindings(extracted: &ExtractedFile) -> Vec<(&str, &str)> {
    extracted
        .import_bindings
        .iter()
        .map(|binding| {
            (
                binding.module_specifier.as_str(),
                binding.local_name.as_str(),
            )
        })
        .collect()
}

fn assert_no_secret(extracted: &ExtractedFile) {
    assert!(
        !format!("{extracted:?}").contains(SECRET_SENTINEL),
        "a source literal leaked into extracted facts"
    );
}

#[test]
fn common_lisp_reader_escapes_abstain_before_designator_facts() {
    for &(path, source) in escaped_names::ESCAPED_NAME_CASES {
        if std::path::Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lisp"))
        {
            let file = credential_support::extract(path, source);
            credential_support::assert_no_credentials(&file);
            assert_eq!(file.symbols.len(), 0);
            assert_eq!(file.import_bindings.len(), 0);
        }
    }
}

#[test]
fn common_lisp_package_designators_screen_credentials() {
    credential_support::assert_screened(
        "main.lisp",
        "(defpackage \"@VALUE@\")\n(in-package \"@VALUE@\")\n",
        "token",
    );
}
