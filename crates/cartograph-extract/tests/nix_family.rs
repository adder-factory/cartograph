//! Nix extraction contracts restored from the v1 Nix extractor.

mod credential_support;
mod dependency_ownership;
mod script_family_support;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::ImportBindingKind;
use script_family_support::{
    ReferenceQuery, assert_linear_work, extract, names_of_kind, numbered_names, symbol,
};

const SECRET_SENTINEL: &str = "sk_live_nix_family_secret";
/// Parentheses around a selection base, deeper than call naming follows.
const DEEP_PARENTHESES: usize = 40;

#[test]
fn nix_v1_fixture_extracts_bindings_signatures_imports_and_apply_calls() {
    let extracted = extract(
        "default.nix",
        "\n{ pkgs ? import <nixpkgs> {} }:\nlet\n  helper = x: builtins.toString x;\n  localValue = helper 1;\nin rec {\n  package = pkgs.stdenv.mkDerivation {\n    name = helper 1;\n  };\n  inherit (pkgs) lib;\n}\n",
    );
    assert_eq!(extracted.language, SourceLanguage::Nix);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    let helper = symbol(&extracted, SymbolKind::Function, "helper");
    assert_eq!(helper.signature.as_deref(), Some("x"));
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        ["lib", "localValue", "name", "package"]
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "name").qualified_name,
        "package::name"
    );
    assert_eq!(names_of_kind(&extracted, SymbolKind::Import), ["<nixpkgs>"]);
    assert!(ReferenceQuery::new(ReferenceKind::Imports, "<nixpkgs>").found_in(&extracted));
    let binding = extracted
        .import_bindings
        .iter()
        .find(|binding| binding.module_specifier == "<nixpkgs>")
        .unwrap_or_else(|| panic!("missing <nixpkgs> binding"));
    assert_eq!(binding.kind, ImportBindingKind::IncludeSystem);
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "builtins.toString")
            .owned_by(&helper.id)
            .found_in(&extracted)
    );
    let local_value = symbol(&extracted, SymbolKind::Constant, "localValue");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "helper")
            .owned_by(&local_value.id)
            .found_in(&extracted)
    );
    let package = symbol(&extracted, SymbolKind::Constant, "package");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "pkgs.stdenv.mkDerivation")
            .owned_by(&package.id)
            .found_in(&extracted)
    );
    assert!(
        extracted.references.iter().any(
            |reference| reference.kind == ReferenceKind::References && reference.name == "pkgs"
        )
    );
}

#[test]
fn nix_attrpaths_functions_inherit_and_relative_imports_keep_full_names() {
    let extracted = extract(
        "nix/a.nix",
        "{ x = 1; f = y: y; a.b.c = 1; \"quoted\" = 2; ${dyn} = 3; inherit (lib.attrs) foo \"bar\"; inherit plain; d = import ./dep.nix; e = import ../lib; h = builtins.import ./b.nix; out = g a (k b); }\n",
    );
    assert_eq!(names_of_kind(&extracted, SymbolKind::Function), ["f"]);
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        [
            "a.b.c", "bar", "d", "e", "foo", "h", "out", "plain", "quoted", "x"
        ]
    );
    assert!(ReferenceQuery::new(ReferenceKind::References, "lib.attrs").found_in(&extracted));
    for specifier in ["./dep.nix", "../lib", "./b.nix"] {
        assert!(ReferenceQuery::new(ReferenceKind::Imports, specifier).found_in(&extracted));
        let binding = extracted
            .import_bindings
            .iter()
            .find(|binding| binding.module_specifier == specifier)
            .unwrap_or_else(|| panic!("missing binding {specifier}"));
        assert_eq!(binding.kind, ImportBindingKind::Namespace);
        assert_eq!(binding.local_name, "<load>");
    }
    let out = symbol(&extracted, SymbolKind::Constant, "out");
    assert_eq!(
        ReferenceQuery::new(ReferenceKind::Calls, "g")
            .owned_by(&out.id)
            .count_in(&extracted),
        1,
        "a curried application is one call"
    );
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "k")
            .owned_by(&out.id)
            .found_in(&extracted),
        "an application passed as an argument is its own call"
    );
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.name.contains("dyn")),
        "interpolated attribute names are dynamic"
    );
}

#[test]
fn nix_declared_attrpaths_skip_interpolated_segments_like_v1() {
    // v1 languages/nix.ts names a binding by its identifier and string
    // segments only, so `packages.${system}.default` declares
    // `packages.default` and owns what its value declares.
    let extracted = extract(
        "flake.nix",
        "{\n  outputs = { self }: {\n    packages.${system}.default = import ./default.nix { inherit pkgs; };\n    ${only} = 1;\n  };\n}\n",
    );
    let package = symbol(&extracted, SymbolKind::Constant, "packages.default");
    assert_eq!(package.qualified_name, "outputs::packages.default");
    assert_eq!(package.span.start_line(), 3);
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "pkgs").qualified_name,
        "outputs::packages.default::pkgs"
    );
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.name.contains("only") || symbol.name.contains("system")),
        "interpolated segments never enter a name: {:?}",
        extracted.symbols
    );
}

#[test]
fn nix_parenthesized_applications_and_bare_relative_paths_are_exact() {
    let extracted = extract(
        "nix/b.nix",
        "{ out = (f x) y; dep = import dep/a.nix; home = import ~/cfg.nix; }\n",
    );
    let out = symbol(&extracted, SymbolKind::Constant, "out");
    assert_eq!(
        ReferenceQuery::new(ReferenceKind::Calls, "f")
            .owned_by(&out.id)
            .count_in(&extracted),
        1,
        "`(f x) y` is one call of `f`"
    );
    let relative = extracted
        .import_bindings
        .iter()
        .find(|binding| binding.module_specifier == "./dep/a.nix")
        .unwrap_or_else(|| panic!("bare relative path must resolve from the file"));
    assert_eq!(relative.kind, ImportBindingKind::Namespace);
    assert!(ReferenceQuery::new(ReferenceKind::Imports, "./dep/a.nix").found_in(&extracted));
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Import),
        ["dep/a.nix", "~/cfg.nix"]
    );
    let home = extracted
        .import_bindings
        .iter()
        .find(|binding| binding.module_specifier == "~/cfg.nix")
        .unwrap_or_else(|| panic!("missing home-path binding"));
    assert_eq!(home.kind, ImportBindingKind::IncludeSystem);
}

#[test]
fn nix_extraction_is_deterministic_and_literal_free() {
    let source = format!(
        "{{ token = \"{SECRET_SENTINEL}\"; open = {{ key ? \"{SECRET_SENTINEL}\" }}: key; pathy = {{ cfg ? ./{SECRET_SENTINEL} }}: cfg; }}\n"
    );
    let first = extract("nix/vault.nix", &source);
    let second = extract("nix/vault.nix", &source);
    assert_eq!(first, second);
    assert!(!format!("{first:?}").contains(SECRET_SENTINEL));
    for function in ["open", "pathy"] {
        assert!(
            symbol(&first, SymbolKind::Function, function)
                .signature
                .is_none(),
            "{function} has a literal default"
        );
    }
}

#[test]
fn nix_identifiers_keep_primes_and_dashes() {
    let extracted = extract(
        "lib/primes.nix",
        "{ lib, ... }:\n{\n  inherit (lib) foldl' go-modules;\n  go' = x: x;\n  r = lib.mapAttrs' (n: v: v) {};\n  s = foldl' (a: b: a) 0 [];\n  t = lib.${name} 1;\n}\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        ["foldl'", "go-modules", "r", "s", "t"]
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "go'")
            .signature
            .as_deref(),
        Some("x")
    );
    let mapped = symbol(&extracted, SymbolKind::Constant, "r");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "lib.mapAttrs'")
            .owned_by(&mapped.id)
            .found_in(&extracted)
    );
    let folded = symbol(&extracted, SymbolKind::Constant, "s");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "foldl'")
            .owned_by(&folded.id)
            .found_in(&extracted)
    );
    // An unnameable selection is not a call of the attribute set it selects from.
    for owner in [mapped, symbol(&extracted, SymbolKind::Constant, "t")] {
        assert!(
            !ReferenceQuery::new(ReferenceKind::Calls, "lib")
                .owned_by(&owner.id)
                .found_in(&extracted),
            "{}: {:?}",
            owner.name,
            extracted.references
        );
    }
}

#[test]
fn nix_quoted_selectors_never_enter_reference_names() {
    let extracted = extract(
        "pkgs/quoted.nix",
        &format!(
            "{{\n  out = pkgs.\"{SECRET_SENTINEL}\" 1;\n  inherit (src.\"{SECRET_SENTINEL}\") x;\n  \"quoted-key\" = 1;\n  run = (pkgs.\"{SECRET_SENTINEL}\").start 1;\n  inherit ((src.\"{SECRET_SENTINEL}\").attrs) y;\n  local = ({{ go = f; }}).go 1;\n  deep = {open}pkgs.\"{SECRET_SENTINEL}\"{close}.launch 1;\n}}\n",
            open = "(".repeat(DEEP_PARENTHESES),
            close = ")".repeat(DEEP_PARENTHESES),
        ),
    );
    // A selection from an unnameable value names nothing, while a selection
    // from a value that is not a name at all keeps v1's attribute name.
    for invented in ["start", "attrs", "launch"] {
        assert!(
            !extracted
                .references
                .iter()
                .any(|reference| reference.name == invented),
            "{invented} is not the whole target: {:?}",
            extracted.references
        );
    }
    let local = symbol(&extracted, SymbolKind::Constant, "local");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "go")
            .owned_by(&local.id)
            .found_in(&extracted)
    );
    assert!(
        !extracted
            .references
            .iter()
            .any(|reference| reference.name.contains(SECRET_SENTINEL)),
        "{:?}",
        extracted.references
    );
    for partial in ["pkgs", "src"] {
        assert!(
            !extracted
                .references
                .iter()
                .any(|reference| reference.name == partial),
            "a quoted selection is not a use of its base {partial}: {:?}",
            extracted.references
        );
    }
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        ["deep", "go", "local", "out", "quoted-key", "run", "x", "y"]
    );
}

#[test]
fn nix_wide_inherits_take_linear_work() {
    assert_linear_work("pkgs/wide.nix", |width| {
        format!(
            "{{ inherit (pkgs) {}; }}\n",
            numbered_names("attr", width, " ")
        )
    });
}

#[test]
fn nix_loads_and_quoted_keys_screen_credentials() {
    credential_support::assert_screened(
        "default.nix",
        "{ \"@VALUE@\" = import \"@VALUE@\"; }\n",
        "token",
    );
}
