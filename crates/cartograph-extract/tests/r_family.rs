//! R extraction contracts restored from the v1 R extractor.

mod credential_support;
mod dependency_ownership;
mod script_family_support;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::ImportBindingKind;
use script_family_support::{
    ReferenceQuery, assert_linear_work, extract, names_of_kind, numbered_names, symbol,
};

const SECRET_SENTINEL: &str = "sk_live_r_family_secret";

#[test]
fn r_assignment_operators_name_functions_from_their_left_hand_side() {
    let extracted = extract(
        "R/main.R",
        "add <- function(a, b) {\n  a + b\n}\nsubtract = function(a, b) a - b\ndivide <<- function(a, b) a / b\nprint.myClass <- function(x, ...) cat(x$value)\n",
    );
    assert_eq!(extracted.language, SourceLanguage::R);
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Function),
        ["add", "divide", "print.myClass", "subtract"]
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "add")
            .signature
            .as_deref(),
        Some("(a, b)")
    );
    let print = symbol(&extracted, SymbolKind::Function, "print.myClass");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "cat")
            .owned_by(&print.id)
            .found_in(&extracted)
    );
}

#[test]
fn r_inline_lambdas_are_not_functions_and_top_level_values_are_constants() {
    let extracted = extract(
        "R/main.R",
        "result <- lapply(xs, function(x) x * 2)\nPI <- 3.14159\nCOLORS <- c(\"red\", \"green\")\nouter <- function() {\n  x <- 5\n  x\n}\nlapply(items, function(item) {\n  scratch <- item\n})\n",
    );
    assert_eq!(names_of_kind(&extracted, SymbolKind::Function), ["outer"]);
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        ["COLORS", "PI", "result"]
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Variable),
        ["scratch", "x"],
        "assignments inside any function, named or not, are locals"
    );
    assert!(
        !symbol(&extracted, SymbolKind::Variable, "scratch")
            .export
            .exported
    );
    let outer = symbol(&extracted, SymbolKind::Function, "outer");
    assert_eq!(
        symbol(&extracted, SymbolKind::Variable, "x").qualified_name,
        "outer::x"
    );
    assert!(
        extracted
            .containments
            .iter()
            .any(|edge| edge.parent == outer.id)
    );
    let result = symbol(&extracted, SymbolKind::Constant, "result");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "lapply")
            .owned_by(&result.id)
            .found_in(&extracted)
    );
}

#[test]
fn r_roxygen_comments_document_functions_and_constants() {
    let extracted = extract(
        "R/main.R",
        "#' Add two numbers\n#' @param a numeric\nadd <- function(a, b) a + b\n#' Maximum rows\nMAX_N <- 100\n",
    );
    assert!(
        symbol(&extracted, SymbolKind::Function, "add")
            .docstring
            .as_deref()
            .is_some_and(|doc| doc.contains("Add two numbers"))
    );
    assert!(
        symbol(&extracted, SymbolKind::Constant, "MAX_N")
            .docstring
            .as_deref()
            .is_some_and(|doc| doc.contains("Maximum rows"))
    );
}

#[test]
fn r_calls_inside_functions_keep_namespaced_names() {
    let extracted = extract(
        "R/main.R",
        "wrap <- function(x) {\n  inner(x)\n  another(x)\n  dplyr::filter(df, x > 0)\n}\n",
    );
    let wrap = symbol(&extracted, SymbolKind::Function, "wrap");
    for name in ["inner", "another", "dplyr::filter"] {
        assert!(
            ReferenceQuery::new(ReferenceKind::Calls, name)
                .owned_by(&wrap.id)
                .found_in(&extracted),
            "{name}"
        );
    }
}

#[test]
fn r_library_require_and_source_become_imports_with_typed_bindings() {
    let extracted = extract(
        "R/main.R",
        "library(dplyr)\nlibrary(\"tidyr\")\nrequire(ggplot2)\nsource(\"helpers.R\")\nsource(r\"(raw.R)\")\nlibrary(R\"[mypkg]\")\nlibrary(r\"{otherpkg}\")\nsource(r\"-(file.R)-\")\nsource(paste0(BASE, \"/dynamic.R\"))\nlibrary(help = \"docsonly\")\nlibrary(pkgvar, character.only = TRUE)\nlibrary(package = named)\nsource(file = \"named.R\")\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Import),
        [
            "dplyr",
            "file.R",
            "ggplot2",
            "helpers.R",
            "mypkg",
            "named",
            "named.R",
            "otherpkg",
            "raw.R",
            "tidyr"
        ]
    );
    for (specifier, kind) in [
        ("dplyr", ImportBindingKind::IncludeSystem),
        ("tidyr", ImportBindingKind::IncludeSystem),
        ("ggplot2", ImportBindingKind::IncludeSystem),
        ("mypkg", ImportBindingKind::IncludeSystem),
        ("named", ImportBindingKind::IncludeSystem),
        ("helpers.R", ImportBindingKind::Namespace),
        ("raw.R", ImportBindingKind::Namespace),
        ("file.R", ImportBindingKind::Namespace),
        ("named.R", ImportBindingKind::Namespace),
    ] {
        assert!(ReferenceQuery::new(ReferenceKind::Imports, specifier).found_in(&extracted));
        let binding = extracted
            .import_bindings
            .iter()
            .find(|binding| binding.module_specifier == specifier)
            .unwrap_or_else(|| panic!("missing binding {specifier}"));
        assert_eq!(binding.kind, kind, "{specifier}");
        let namespace = if kind == ImportBindingKind::IncludeSystem {
            specifier
        } else {
            "<load>"
        };
        assert_eq!(binding.local_name, namespace, "{specifier}");
    }
    for skipped in ["docsonly", "pkgvar"] {
        assert!(
            !extracted
                .import_bindings
                .iter()
                .any(|binding| binding.module_specifier == skipped),
            "{skipped} does not attach a statically named package"
        );
    }
    assert!(
        !extracted
            .import_bindings
            .iter()
            .any(|binding| binding.module_specifier.contains("dynamic")),
        "dynamic source() paths are never guessed"
    );
    for call in ["library", "require", "source"] {
        assert!(ReferenceQuery::new(ReferenceKind::Calls, call).found_in(&extracted));
    }
}

#[test]
fn r_chained_assignments_and_computed_callees_keep_top_level_scope_and_literal_free_names() {
    let extracted = extract(
        "R/chain.R",
        "a <- b <- compute()\nc <- (d <- 2)\nrun <- function() {\n  fetch(12345)$run()\n  pkg::helper()\n  obj$member$method()\n  dplyr :: filter(df)\n  (wrapped)$call()\n}\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        ["a", "b", "c", "d"]
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "d").qualified_name,
        "d"
    );
    let b = symbol(&extracted, SymbolKind::Constant, "b");
    assert_eq!(b.qualified_name, "b");
    assert!(b.export.exported);
    assert_eq!(
        ReferenceQuery::new(ReferenceKind::Calls, "compute")
            .owned_by(&b.id)
            .count_in(&extracted),
        1
    );
    let run = symbol(&extracted, SymbolKind::Function, "run");
    for name in [
        "fetch",
        "pkg::helper",
        "obj$member$method",
        "dplyr::filter",
        "wrapped$call",
    ] {
        assert!(
            ReferenceQuery::new(ReferenceKind::Calls, name)
                .owned_by(&run.id)
                .found_in(&extracted),
            "{name}"
        );
    }
    assert!(
        !extracted
            .references
            .iter()
            .any(|reference| reference.name.contains("12345")),
        "computed callees never carry argument literals: {:?}",
        extracted.references
    );
}

#[test]
fn r_extraction_is_deterministic_and_literal_free() {
    let source = format!(
        "TOKEN <- \"{SECRET_SENTINEL}\"\nopen <- function(key = \"{SECRET_SENTINEL}\") check(\"{SECRET_SENTINEL}\")\n"
    );
    let first = extract("R/vault.R", &source);
    let second = extract("R/vault.R", &source);
    assert_eq!(first, second);
    assert!(!format!("{first:?}").contains(SECRET_SENTINEL));
    assert!(
        symbol(&first, SymbolKind::Function, "open")
            .signature
            .is_none()
    );
}

#[test]
fn r_character_only_loads_keep_literal_package_names() {
    let extracted = extract(
        "R/load.R",
        "library(\"dplyr\", character.only = TRUE)\nlibrary(tidyr, character.only = FALSE)\nlibrary(pkgvar, character.only = TRUE)\nrequire(dynamic, character.only = flag)\nF <- TRUE\nlibrary(shadowed, character.only = F)\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Import),
        ["dplyr", "tidyr"]
    );
}

#[test]
fn r_long_assignment_chains_take_linear_work() {
    assert_linear_work("R/chain.R", |width| {
        format!("{} <- 1\n", numbered_names("link", width, " <- "))
    });
}

#[test]
fn r_load_urls_retain_ordinary_token_path_facts() {
    let uri = "https://example.invalid/token/helpers.R";
    let file = extract("main.R", &format!("source(\"{uri}\")\n"));
    assert_eq!(names_of_kind(&file, SymbolKind::Import), [uri]);
    assert_eq!(symbol(&file, SymbolKind::Import, uri).qualified_name, uri);
    assert!(
        file.references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::Imports && reference.name == uri)
    );
    assert!(
        file.import_bindings
            .iter()
            .any(|binding| binding.kind == ImportBindingKind::Namespace
                && binding.module_specifier == uri
                && binding.imported_name == "*")
    );
}

#[test]
fn r_loads_screen_credentials_before_emitting_import_facts() {
    credential_support::assert_screened("main.R", "source(\"@VALUE@\")\n", "token");
}
