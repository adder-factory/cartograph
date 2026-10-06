//! Integration coverage for Cartograph native extraction contracts.

use std::fmt::Write as _;

mod dependency_ownership;

use cartograph_domain::{SourceLanguage, SymbolKind};
use cartograph_extract::{
    NativeExtractor, NativeGrammar, SourceLimits, SourceSnapshot, native_extraction_reservation,
    native_output_limit,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
const SECRET_SENTINEL: &str = "sk_live_generic_family_secret";

struct Fixture {
    language: SourceLanguage,
    path: &'static str,
    source: &'static str,
    expected_name_fragment: &'static str,
}

const FIXTURES: [Fixture; 28] = [
    Fixture {
        language: SourceLanguage::Abap,
        path: "src/beacon.abap",
        source: "CLASS zcl_beacon DEFINITION.\n PUBLIC SECTION.\n METHODS run_beacon.\nENDCLASS.\nCLASS zcl_beacon IMPLEMENTATION.\n METHOD run_beacon.\n ENDMETHOD.\nENDCLASS.\n",
        expected_name_fragment: "beacon",
    },
    Fixture {
        language: SourceLanguage::Apex,
        path: "force-app/main/default/classes/ApexBeacon.cls",
        source: "public class ApexBeacon { public static void runBeacon() {} }\n",
        expected_name_fragment: "Beacon",
    },
    Fixture {
        language: SourceLanguage::ArkTs,
        path: "src/ArkBeacon.ets",
        source: "export function arkBeacon(): void {}\n",
        expected_name_fragment: "arkBeacon",
    },
    Fixture {
        language: SourceLanguage::Astro,
        path: "src/AstroBeacon.astro",
        source: "---\nconst AstroBeacon = 'safe';\n---\n<CustomBeacon />\n",
        expected_name_fragment: "Beacon",
    },
    Fixture {
        language: SourceLanguage::Clojure,
        path: "src/beacon.clj",
        source: "(ns beacon.core)\n(defn clojureBeacon [] 1)\n",
        expected_name_fragment: "clojureBeacon",
    },
    Fixture {
        language: SourceLanguage::CommonLisp,
        path: "src/beacon.lisp",
        source: "(defpackage :beacon)\n(in-package :beacon)\n(defun lisp-beacon () 1)\n",
        expected_name_fragment: "lisp-beacon",
    },
    Fixture {
        language: SourceLanguage::Dart,
        path: "lib/dart_beacon.dart",
        source: "class DartBeacon { void runBeacon() {} }\n",
        expected_name_fragment: "DartBeacon",
    },
    Fixture {
        language: SourceLanguage::FSharp,
        path: "src/Beacon.fs",
        source: "module Beacon\nlet fsharpBeacon value = value\n",
        expected_name_fragment: "Beacon",
    },
    Fixture {
        language: SourceLanguage::GraphQl,
        path: "schema/beacon.graphql",
        source: "type GraphBeacon { beaconField: String! }\n",
        expected_name_fragment: "GraphBeacon",
    },
    Fixture {
        language: SourceLanguage::Hcl,
        path: "infra/beacon.tf",
        source: "resource \"null_resource\" \"hcl_beacon\" { triggers = { safe = \"yes\" } }\n",
        expected_name_fragment: "resource",
    },
    Fixture {
        language: SourceLanguage::Html,
        path: "web/beacon.html",
        source: "<custom-beacon></custom-beacon>\n",
        expected_name_fragment: "custom-beacon",
    },
    Fixture {
        language: SourceLanguage::Khn,
        path: "Scripts/beacon.khn",
        source: "function khnBeacon() return 1 end\n",
        expected_name_fragment: "khnBeacon",
    },
    Fixture {
        language: SourceLanguage::Lean,
        path: "src/Beacon.lean",
        source: "def leanBeacon : Nat := 1\n",
        expected_name_fragment: "leanBeacon",
    },
    Fixture {
        language: SourceLanguage::Lua,
        path: "src/beacon.lua",
        source: "function luaBeacon() return 1 end\n",
        expected_name_fragment: "luaBeacon",
    },
    Fixture {
        language: SourceLanguage::Luau,
        path: "src/beacon.luau",
        source: "local function luauBeacon(): number return 1 end\n",
        expected_name_fragment: "luauBeacon",
    },
    Fixture {
        language: SourceLanguage::Nix,
        path: "nix/beacon.nix",
        source: "{ nixBeacon = 1; }\n",
        expected_name_fragment: "nixBeacon",
    },
    Fixture {
        language: SourceLanguage::ObjectiveC,
        path: "src/ObjcBeacon.m",
        source: "@interface ObjcBeacon : NSObject\n- (void)runBeacon;\n@end\n@implementation ObjcBeacon\n- (void)runBeacon {}\n@end\n",
        expected_name_fragment: "ObjcBeacon",
    },
    Fixture {
        language: SourceLanguage::Pascal,
        path: "src/beacon.pas",
        source: "program Beacon;\nprocedure pascalBeacon; begin end;\nbegin pascalBeacon; end.\n",
        expected_name_fragment: "Beacon",
    },
    Fixture {
        language: SourceLanguage::Php,
        path: "src/PhpBeacon.php",
        source: "<?php class PhpBeacon { public function runBeacon() { $token = 'sk_live_generic_family_secret'; } }\n",
        expected_name_fragment: "PhpBeacon",
    },
    Fixture {
        language: SourceLanguage::Prisma,
        path: "prisma/beacon.prisma",
        source: "model PrismaBeacon { id Int @id }\n",
        expected_name_fragment: "PrismaBeacon",
    },
    Fixture {
        language: SourceLanguage::R,
        path: "R/beacon.r",
        source: "rBeacon <- function(value) value\n",
        expected_name_fragment: "rBeacon",
    },
    Fixture {
        language: SourceLanguage::ReScript,
        path: "src/Beacon.res",
        source: "let rescriptBeacon = () => ()\n",
        expected_name_fragment: "rescriptBeacon",
    },
    Fixture {
        language: SourceLanguage::Ruby,
        path: "lib/ruby_beacon.rb",
        source: "class RubyBeacon\n  def run_beacon\n    1\n  end\nend\n",
        expected_name_fragment: "RubyBeacon",
    },
    Fixture {
        language: SourceLanguage::Solidity,
        path: "contracts/Beacon.sol",
        source: "contract SolidityBeacon { function runBeacon() public pure returns (uint) { return 1; } }\n",
        expected_name_fragment: "SolidityBeacon",
    },
    Fixture {
        language: SourceLanguage::Sql,
        path: "db/beacon.sql",
        source: "CREATE TABLE sql_beacon (id INTEGER PRIMARY KEY);\n",
        expected_name_fragment: "sql_beacon",
    },
    Fixture {
        language: SourceLanguage::Swift,
        path: "Sources/SwiftBeacon.swift",
        source: "public struct SwiftBeacon { public func runBeacon() {} }\n",
        expected_name_fragment: "SwiftBeacon",
    },
    Fixture {
        language: SourceLanguage::VbNet,
        path: "src/VbBeacon.vb",
        source: "Public Class VbBeacon\n  Public Sub RunBeacon()\n  End Sub\nEnd Class\n",
        expected_name_fragment: "VbBeacon",
    },
    Fixture {
        language: SourceLanguage::Yaml,
        path: "config/beacon.yaml",
        source: "yamlBeacon:\n  enabled: true\n",
        expected_name_fragment: "yamlBeacon",
    },
];

#[test]
fn remaining_grammar_families_are_production_admitted_and_emit_bounded_symbols() {
    for fixture in FIXTURES {
        let snapshot =
            SourceSnapshot::from_bytes(fixture.path, fixture.source.as_bytes(), limits())
                .unwrap_or_else(|error| panic!("{} snapshot failed: {error}", fixture.path));
        assert_eq!(snapshot.language(), fixture.language, "{}", fixture.path);
        assert!(fixture.language.is_native_indexable());
        let mut extractor = NativeExtractor::new(fixture.language)
            .unwrap_or_else(|error| panic!("{} extractor failed: {error}", fixture.path));
        let first = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("{} extraction failed: {error}", fixture.path));
        let second = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("{} repeat failed: {error}", fixture.path));
        assert_eq!(first, second, "{} was not deterministic", fixture.path);
        assert!(
            first.symbols.iter().any(|symbol| {
                symbol
                    .qualified_name
                    .to_ascii_lowercase()
                    .contains(&fixture.expected_name_fragment.to_ascii_lowercase())
            }),
            "{} did not emit expected structural name; symbols={:?}; ast={}",
            fixture.path,
            first
                .symbols
                .iter()
                .map(|symbol| (&symbol.qualified_name, symbol.kind))
                .collect::<Vec<_>>(),
            syntax_tree(&fixture)
        );
        let rendered = format!("{first:?}");
        assert!(
            !rendered.contains(SECRET_SENTINEL),
            "{} leaked a literal",
            fixture.path
        );
    }
}

#[test]
fn dense_yaml_uses_separate_working_and_retained_output_bounds() {
    let mut source = String::from("root:\n");
    for index in 0..1_500 {
        writeln!(source, "  field_{index}: value_{index}")
            .unwrap_or_else(|error| panic!("could not construct dense YAML fixture: {error}"));
    }
    let snapshot = SourceSnapshot::from_bytes("fixtures/dense.yaml", source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("dense YAML snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(SourceLanguage::Yaml)
        .unwrap_or_else(|error| panic!("YAML extractor failed: {error}"));
    let extracted = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("dense YAML extraction failed: {error}"));
    let source_bytes = u64::try_from(source.len())
        .unwrap_or_else(|error| panic!("dense YAML source length overflowed: {error}"));
    let retained = extracted.modeled_retained_bytes();
    let retained_limit = native_output_limit(source_bytes)
        .unwrap_or_else(|| panic!("dense YAML retained limit overflowed"));
    let working_limit = native_extraction_reservation(source_bytes)
        .unwrap_or_else(|| panic!("dense YAML working limit overflowed"));
    let previous_combined_limit = source_bytes
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(256 * 1024))
        .unwrap_or_else(|| panic!("dense YAML previous limit overflowed"));

    assert!(extracted.symbols.len() >= 1_500);
    assert!(retained > previous_combined_limit);
    assert!(retained <= retained_limit, "{retained} > {retained_limit}");
    assert!(retained_limit < working_limit);
}

#[test]
fn generic_family_cancellation_is_fail_closed() {
    let fixture = &FIXTURES[0];
    let snapshot = SourceSnapshot::from_bytes(fixture.path, fixture.source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("cancellation snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(fixture.language)
        .unwrap_or_else(|error| panic!("cancellation extractor failed: {error}"));
    assert!(
        extractor
            .extract_with_cancellation(&snapshot, || true)
            .is_err()
    );
}

#[test]
fn abap_classes_and_interfaces_are_top_level_not_swallowed_by_the_program_root() {
    let extracted = extract_generic(
        "src/zcl_greeter.clas.abap",
        "CLASS zcl_greeter DEFINITION PUBLIC.\n  PUBLIC SECTION.\n    METHODS greet IMPORTING iv_name TYPE string.\nENDCLASS.\nCLASS zcl_greeter IMPLEMENTATION.\n  METHOD greet.\n    WRITE iv_name.\n  ENDMETHOD.\nENDCLASS.\n",
    );
    let class = generic_symbol(&extracted, SymbolKind::Class, "zcl_greeter");
    assert_eq!(class.qualified_name, "zcl_greeter");
    let method = generic_symbol(&extracted, SymbolKind::Method, "greet");
    assert_eq!(method.qualified_name, "zcl_greeter::greet");
    assert!(
        extracted.containments.iter().any(|containment| {
            containment.parent == class.id && containment.child == method.id
        })
    );

    let interface = extract_generic(
        "src/zif_greeter.intf.abap",
        "INTERFACE zif_greeter PUBLIC.\n  METHODS greet.\nENDINTERFACE.\nCLASS lcl_impl DEFINITION.\n  PUBLIC SECTION.\nENDCLASS.\n",
    );
    let declared = generic_symbol(&interface, SymbolKind::Interface, "zif_greeter");
    assert_eq!(declared.qualified_name, "zif_greeter");
    let local_class = generic_symbol(&interface, SymbolKind::Class, "lcl_impl");
    assert_eq!(local_class.qualified_name, "lcl_impl");

    for extracted in [&extracted, &interface] {
        assert!(
            extracted
                .symbols
                .iter()
                .all(|symbol| symbol.kind != SymbolKind::Module),
            "a compilation-unit root must never become a module: {:?}",
            extracted.symbols
        );
    }
}

/// v1.1.33 declared an ABAP class and its methods at the `IMPLEMENTATION`
/// block. A definition-side declaration first would anchor the symbol on a
/// one-line signature, merge the method body into it, and name the
/// `METHODS constructor IMPORTING iv_prefix` declaration after its parameter.
/// The implementation-anchored class still carries the definition's
/// modifiers and attributes, its implemented methods own their bodies, and
/// a method the implementation does not implement stays a body-less method.
#[test]
fn abap_classes_and_methods_are_declared_at_their_implementation() {
    let source = "CLASS zcl_greeter DEFINITION PUBLIC.\n  PUBLIC SECTION.\n    METHODS constructor IMPORTING iv_prefix TYPE string.\n    \"! Greets the caller.\n    METHODS greet IMPORTING iv_name TYPE string.\n    METHODS later.\n  PRIVATE SECTION.\n    DATA mv_prefix TYPE string.\nENDCLASS.\n\nCLASS ZCL_GREETER IMPLEMENTATION.\n  METHOD constructor.\n    mv_prefix = iv_prefix.\n  ENDMETHOD.\n  METHOD greet.\n    DATA lv_text TYPE string.\n    lv_text = iv_name.\n  ENDMETHOD.\n  METHOD zif_greeter~greet.\n    rv_text = iv_name.\n  ENDMETHOD.\n  METHOD zif_greeter~farewell.\n    WRITE 'Bye'.\n  ENDMETHOD.\nENDCLASS.\n\nCLASS lcl_deferred DEFINITION.\n  PUBLIC SECTION.\n    METHODS run.\nENDCLASS.\n";
    let extracted = extract_generic("src/zcl_greeter.clas.abap", source);
    let declared = extracted
        .symbols
        .iter()
        .map(|symbol| {
            (
                symbol.kind,
                symbol.qualified_name.as_str(),
                symbol.span.start_line(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        [
            (SymbolKind::Class, "ZCL_GREETER", 11),
            (SymbolKind::Method, "ZCL_GREETER::later", 6),
            (SymbolKind::Variable, "ZCL_GREETER::mv_prefix", 8),
            (SymbolKind::Method, "ZCL_GREETER::constructor", 12),
            (SymbolKind::Method, "ZCL_GREETER::greet", 15),
            (SymbolKind::Variable, "ZCL_GREETER::greet::lv_text", 16),
            (SymbolKind::Method, "ZCL_GREETER::zif_greeter", 19),
            (SymbolKind::Method, "ZCL_GREETER::zif_greeter", 22),
            (SymbolKind::Class, "lcl_deferred", 27),
            (SymbolKind::Method, "lcl_deferred::run", 29),
        ]
    );
    let class = generic_symbol(&extracted, SymbolKind::Class, "ZCL_GREETER");
    assert!(
        class.export.exported,
        "the definition's PUBLIC makes the implementation-anchored class exported"
    );
    let greet = generic_symbol(&extracted, SymbolKind::Method, "greet");
    assert!(
        !greet.implementation.declaration_only,
        "an implemented method has its method_body"
    );
    assert!(
        greet
            .docstring
            .as_deref()
            .is_some_and(|doc| doc.contains("Greets the caller")),
        "the implementation keeps its declaration's doc comment: {:?}",
        greet.docstring
    );
    for body_less in ["later", "run"] {
        assert!(
            generic_symbol(&extracted, SymbolKind::Method, body_less)
                .implementation
                .declaration_only,
            "{body_less} has no implementation in the file"
        );
    }
}

fn extract_generic(path: &str, source: &str) -> cartograph_extract::ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"))
}

fn generic_symbol<'file>(
    extracted: &'file cartograph_extract::ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind:?} {name}: {:?}", extracted.symbols))
}

#[test]
fn compilation_unit_programs_do_not_pollute_dart_r_php_or_ruby_qualified_names() {
    for (path, source, forbidden_module, expected) in [
        (
            "lib/models.dart",
            "class Box<T> { final T value; Box(this.value); }\nint size() => 1;\n",
            "int",
            "Box",
        ),
        (
            "R/model.r",
            "source('helpers.R')\nprocess <- function(input) input\n",
            "source",
            "process",
        ),
        (
            "src/model.php",
            "<?php function loadModel() { return 1; }\n",
            "loadModel",
            "loadModel",
        ),
        (
            "lib/model.rb",
            "def load_model\n  1\nend\n",
            "load_model",
            "load_model",
        ),
    ] {
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
            .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"));
        assert!(
            extracted.symbols.iter().any(|symbol| {
                symbol.qualified_name == expected
                    && matches!(
                        symbol.kind,
                        cartograph_domain::SymbolKind::Class
                            | cartograph_domain::SymbolKind::Function
                            | cartograph_domain::SymbolKind::Method
                    )
            }),
            "{path} missing {expected}: {:?}",
            extracted.symbols
        );
        assert!(
            extracted.symbols.iter().all(|symbol| {
                symbol.kind != cartograph_domain::SymbolKind::Module
                    || symbol.qualified_name != forbidden_module
            }),
            "{path} retained compilation-unit module {forbidden_module}: {:?}",
            extracted.symbols
        );
    }
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("generic-family source limit failed: {error}"))
}

fn syntax_tree(fixture: &Fixture) -> String {
    let grammar = NativeGrammar::for_source_language(fixture.language)
        .unwrap_or_else(|| panic!("{} has no diagnostic grammar", fixture.path));
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&grammar.language())
        .unwrap_or_else(|error| panic!("{} grammar failed: {error}", fixture.path));
    parser.parse(fixture.source, None).map_or_else(
        || "<parser stopped>".to_owned(),
        |tree| tree.root_node().to_sexp(),
    )
}
