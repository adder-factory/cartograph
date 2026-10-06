//! v1 parity for the shared grammar-generic walker: anonymous function values
//! are never named after a parameter or body, and recognized imports declare
//! `Import` symbols.

mod credential_support;
mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"))
}

fn callable_names(file: &ExtractedFile) -> Vec<&str> {
    file.symbols
        .iter()
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|symbol| symbol.qualified_name.as_str())
        .collect()
}

#[test]
fn anonymous_function_values_are_not_named_after_parameters_or_bodies() {
    let khn = extract(
        "Scripts/anon.khn",
        "local cb = function(q) return q end\nlocal function named(a) return a end\nt.f = function(v) end\nreturn function(x) end\n",
    );
    // KHN runs on the Lua family: as in v1, an assigned function value is named by
    // its binding (`cb`, `t.f`), never by its first parameter, and a returned
    // anonymous function is not named at all.
    assert_eq!(
        callable_names(&khn),
        ["cb", "named", "t.f"],
        "{:?}",
        khn.symbols
    );
    assert!(
        !khn.symbols
            .iter()
            .any(|symbol| matches!(symbol.name.as_str(), "q" | "v" | "x"))
    );
    let r = extract(
        "R/math.r",
        "add <- function(a, b) a + b\nmapped <- lapply(xs, function(v) v)\n",
    );
    assert_eq!(callable_names(&r), ["add"], "{:?}", r.symbols);
    let objc = extract("src/helpers.m", "int helper(int value) { return value; }\n");
    assert_eq!(
        callable_names(&objc),
        ["helper"],
        "declarator-named C functions are not anonymous: {:?}",
        objc.symbols
    );
    assert!(
        r.references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::Calls && reference.name == "lapply"),
        "calls inside anonymous functions are still captured"
    );
}

#[test]
fn recognized_generic_imports_declare_import_symbols() {
    for (path, source, module) in [
        (
            "Sources/App.swift",
            "import Foundation\nfunc run() {}\n",
            "Foundation",
        ),
        (
            "src/App.vb",
            "Imports System\nPublic Class App\nEnd Class\n",
            "System",
        ),
        (
            "contracts/App.sol",
            "import \"./math.sol\";\ncontract App {}\n",
            "./math.sol",
        ),
        (
            "src/app.php",
            "<?php\ninclude \"./math.php\";\n",
            "./math.php",
        ),
    ] {
        let file = extract(path, source);
        let imports = file
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Import)
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(imports, [module], "{path}: {:?}", file.symbols);
        assert!(
            file.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Imports && reference.name == module
            }),
            "{path} lost its imports edge"
        );
        let import = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Import)
            .unwrap_or_else(|| panic!("{path} import missing"));
        assert_eq!(import.signature, None, "{path} retained raw import text");
        assert!(
            !import.export.exported,
            "{path} import must not be exported"
        );
    }
}

#[test]
fn credential_bearing_import_specifiers_leave_no_import_facts() {
    for (path, source, secret) in [
        (
            "contracts/Leaky.sol",
            "import \"https://reader:FAKEPASSWORDxyz@example.invalid/lib.sol\";\ncontract Leaky {}\n",
            "FAKEPASSWORDxyz",
        ),
        (
            "src/leaky.php",
            "<?php\ninclude \"https://admin:S3cretPassw0rd@example.invalid/m.php\";\n",
            "S3cretPassw0rd",
        ),
        (
            "contracts/Keyed.sol",
            "import \"https://example.invalid/AKIAIOSFODNN7EXAMPLE/lib.sol\";\ncontract Keyed {}\n",
            "AKIAIOSFODNN7EXAMPLE",
        ),
    ] {
        let file = extract(path, source);
        let rendered = format!("{:?}{:?}", file.symbols, file.references);
        assert!(!rendered.contains(secret), "{path} leaked {secret}");
        assert!(
            !file
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Import),
            "{path} declared an Import symbol for a credential-bearing specifier"
        );
        assert!(
            !file
                .references
                .iter()
                .any(|reference| reference.kind == ReferenceKind::Imports),
            "{path} kept an imports edge for a credential-bearing specifier"
        );
    }
    // Scoped packages and credential words in plain module paths are kept.
    let scoped = extract(
        "contracts/Coin.sol",
        "import \"@openzeppelin/contracts/token/ERC20/ERC20.sol\";\ncontract Coin {}\n",
    );
    assert!(
        scoped.symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Import
                && symbol.name == "@openzeppelin/contracts/token/ERC20/ERC20.sol"
        }),
        "{:?}",
        scoped.symbols
    );
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("generic parity source limit failed: {error}"))
}

#[test]
fn solidity_pragma_excludes_comments_and_screens_retained_tokens() {
    for value in credential_support::CREDENTIAL_INPUTS {
        let file = extract("main.sol", &format!("pragma solidity /*{value}*/ ^0.8.0;"));
        credential_support::assert_no_credentials(&file);
        assert!(
            file.symbols.iter().any(|symbol| {
                symbol.kind == SymbolKind::Import && symbol.name == "pragma solidity ^0.8.0"
            }),
            "{file:?}"
        );
    }
    credential_support::assert_screened("main.sol", "pragma @VALUE@;", "experimental ABIEncoderV2");
}
