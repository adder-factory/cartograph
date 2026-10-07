use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, ReferenceKind, build_capability_generation,
    capability_symbol,
};

const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

#[test]
fn rust_review_01_comment_and_literal_headers_are_opaque() {
    for header in [
        "fn run</* { */ Tool: Target>()",
        "fn run<// {\n Tool: Target>()",
        "#[attribute(\"{\")] fn run<Tool: Target>()",
        "#[attribute({})] fn run<Tool: Target>()",
        "#[attribute([nested], {})] fn run<Tool: Target>()",
        "fn run<const C: usize = { 0 }, Tool: Target>()",
        "fn run<const C: char = '{', Tool: Target>()",
    ] {
        let source = format!(
            "pub mod api; use crate::api as Tool; trait Target {{ fn resolve(); }} {header} {{ Tool::resolve(); }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/api.rs", "pub fn resolve() {}"),
            ],
            false,
        );
        unresolved(
            &facts,
            ("src/lib.rs", "run", "Tool::resolve", ReferenceKind::Calls),
        );
    }
}

#[test]
fn rust_review_02_unicode_generic_headers_are_opaque() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub mod api; use crate::api as K; trait Target { fn resolve(); } fn run<\u{212a}: Target>() { K::resolve(); }",
            ),
            ("src/api.rs", "pub fn resolve() {}"),
        ],
        false,
    );
    unresolved(
        &facts,
        ("src/lib.rs", "run", "K::resolve", ReferenceKind::Calls),
    );
}

#[test]
fn rust_review_lifetime_headers_keep_exact_use_targets() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub mod api; use crate::api as Tool; pub fn run<'a>(value: &'a str) { Tool::resolve(); }",
            ),
            ("src/api.rs", "pub fn resolve() {}"),
        ],
        false,
    );
    resolved(
        &facts,
        ("src/lib.rs", "run", "Tool::resolve", ReferenceKind::Calls),
        ("src/api.rs", "resolve", "native-import-binding"),
    );
}

#[test]
fn rust_review_array_semicolons_and_function_arrows_keep_complete_headers() {
    for parameters in [
        "value: [u8; 4]",
        "value: fn() -> u8",
        "value: Vec<fn() -> u8>",
        "Parts { field }: Parts",
    ] {
        let source = format!(
            "pub mod api; use crate::api as Tool; pub struct Parts {{ field: u8 }} pub fn run({parameters}) {{ Tool::resolve(); }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/api.rs", "pub fn resolve() {}"),
            ],
            false,
        );
        resolved(
            &facts,
            ("src/lib.rs", "run", "Tool::resolve", ReferenceKind::Calls),
            ("src/api.rs", "resolve", "native-import-binding"),
        );
    }
}

#[test]
fn rust_review_03_absolute_paths_never_select_local_namesakes() {
    let facts = build_capability_generation(
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app\", \"tools\"]\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ntools = { path = \"../tools\" }\n",
            ),
            (
                "tools/Cargo.toml",
                "[package]\nname = \"tools\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "app/src/lib.rs",
                "mod tools; pub fn run() { let _ = ::tools::Slot {}; }",
            ),
            ("app/src/tools.rs", "pub struct Slot {}"),
            ("tools/src/lib.rs", "pub struct Slot {}"),
        ],
        false,
    );
    resolved(
        &facts,
        ("app/src/lib.rs", "run", "Slot", ReferenceKind::Instantiates),
        ("tools/src/lib.rs", "Slot", "native-rust-workspace-crate"),
    );
}

#[test]
fn rust_review_04_raw_leaf_paths_keep_inline_self_and_super_anchors() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub fn r#match() {} pub mod nested { pub fn r#match() {} pub fn run() { self::r#match(); } pub mod deep { pub fn run() { super::r#match(); } } }",
            ),
        ],
        false,
    );
    for (owner, name) in [
        ("nested::run", "self::r#match"),
        ("nested::deep::run", "super::r#match"),
    ] {
        resolved(
            &facts,
            ("src/lib.rs", owner, name, ReferenceKind::Calls),
            (
                "src/lib.rs",
                "nested::r#match",
                "native-rust-qualified-path",
            ),
        );
    }
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod r#api; pub fn run() { r#api::helper(); }"),
            ("src/api.rs", "pub fn helper() {}"),
        ],
        false,
    );
    unresolved(
        &facts,
        ("src/lib.rs", "run", "r#api::helper", ReferenceKind::Calls),
    );
    for (declaration, call) in [("helper", "r#helper"), ("r#helper", "helper")] {
        let source = format!(
            "pub fn helper() {{}} pub mod nested {{ pub fn {declaration}() {{}} pub fn run() {{ self::{call}(); }} }}"
        );
        let facts = build_capability_generation(
            &[("Cargo.toml", MANIFEST), ("src/lib.rs", &source)],
            false,
        );
        resolved(
            &facts,
            (
                "src/lib.rs",
                "nested::run",
                &format!("self::{call}"),
                ReferenceKind::Calls,
            ),
            (
                "src/lib.rs",
                &format!("nested::{declaration}"),
                "native-rust-qualified-path",
            ),
        );
    }
}

#[test]
fn rust_review_raw_candidates_do_not_enter_weaker_lexical_tiers() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub fn r#helper() {} pub fn run() { let helper = || {}; helper(); }",
            ),
        ],
        false,
    );
    unresolved(
        &facts,
        ("src/lib.rs", "run", "helper", ReferenceKind::Calls),
    );
}

#[test]
fn rust_review_raw_leaf_alternatives_are_ambiguous_before_visibility() {
    for (public, private) in [("r#helper", "helper"), ("helper", "r#helper")] {
        let source = format!(
            "pub mod tools {{ #[cfg(a)] pub fn {public}() {{}} #[cfg(b)] fn {private}() {{}} }} use crate::tools::helper as imported; pub fn run() {{ crate::tools::r#helper(); crate::tools::helper(); imported(); }}"
        );
        let facts = build_capability_generation(
            &[("Cargo.toml", MANIFEST), ("src/lib.rs", &source)],
            false,
        );
        for name in ["crate::tools::r#helper", "crate::tools::helper", "imported"] {
            unresolved(&facts, ("src/lib.rs", "run", name, ReferenceKind::Calls));
        }
    }
}

#[test]
fn rust_review_private_raw_declarations_cannot_fall_through_to_reexports() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub mod tools { #[cfg(a)] fn r#helper() {} #[cfg(b)] pub use crate::other::r#helper; } pub mod other { pub fn r#helper() {} } pub fn run() { crate::tools::r#helper(); crate::tools::helper(); }",
            ),
        ],
        false,
    );
    for name in ["crate::tools::r#helper", "crate::tools::helper"] {
        unresolved(&facts, ("src/lib.rs", "run", name, ReferenceKind::Calls));
    }
}

#[test]
fn rust_review_05_file_and_inline_module_alternatives_are_ambiguous() {
    for file_declaration in ["mod tools;", "#[cfg(feature = \"file\")] mod tools;"] {
        let source = format!(
            "{file_declaration} #[cfg(not(feature = \"file\"))] pub mod tools {{ pub fn helper() {{}} }} pub fn run() {{ tools::helper(); crate::tools::helper(); }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/tools.rs", "pub fn helper() {}"),
            ],
            false,
        );
        for name in ["tools::helper", "crate::tools::helper"] {
            unresolved(&facts, ("src/lib.rs", "run", name, ReferenceKind::Calls));
        }
    }
}

#[test]
fn rust_review_file_modules_and_nominal_alternatives_are_ambiguous() {
    for declaration in ["mod tools;", "#[cfg(a)] mod tools;"] {
        let source = format!(
            "{declaration} #[cfg(b)] pub struct tools; #[cfg(b)] impl tools {{ pub fn helper() {{}} }} pub fn run() {{ tools::helper(); }}"
        );
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/tools.rs", "pub fn helper() {}"),
            ],
            false,
        );
        unresolved(
            &facts,
            ("src/lib.rs", "run", "tools::helper", ReferenceKind::Calls),
        );
    }
}

#[test]
fn rust_review_ambiguous_module_type_imports_cannot_bind_inline_namesakes() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "#[cfg(a)] mod tools; #[cfg(b)] pub mod tools { pub struct Slot {} } use self::tools::Slot; pub struct Holder { value: Slot }",
            ),
            ("src/tools.rs", "pub struct Slot {}"),
        ],
        false,
    );
    unresolved(
        &facts,
        ("src/lib.rs", "Holder", "Slot", ReferenceKind::TypeOf),
    );
}

#[test]
fn rust_review_06_guarded_standard_modules_keep_statement_macro_fences() {
    for namespace in ["std", "core"] {
        let source = format!(
            "mod api; use crate::api::helper; #[cfg(all())] mod {namespace}; pub fn run() {{ {namespace}::assert!(true); helper(); }}"
        );
        let path = format!("src/{namespace}.rs");
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/api.rs", "pub fn helper() {}"),
                (
                    &path,
                    "#[macro_export] macro_rules! custom_assert { ($e:expr) => { fn helper() {} } } pub use crate::custom_assert as assert;",
                ),
            ],
            false,
        );
        unresolved(
            &facts,
            ("src/lib.rs", "run", "helper", ReferenceKind::Calls),
        );
    }
}

#[test]
fn rust_review_standard_namespace_items_and_aliases_fence_the_crate() {
    for declaration in [
        "pub struct std;",
        "pub mod std {}",
        "use crate::custom as std;",
        "use crate::custom as r#std;",
        "#[cfg(all())] mod std;",
    ] {
        let source = format!("mod api; mod caller; mod custom; {declaration}");
        let facts = build_capability_generation(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", &source),
                ("src/api.rs", "pub fn helper() {}"),
                (
                    "src/caller.rs",
                    "use crate::api::helper; pub fn run() { std::assert!(true); helper(); }",
                ),
                ("src/custom.rs", "pub fn unrelated() {}"),
                ("src/std.rs", "pub fn unrelated() {}"),
            ],
            false,
        );
        unresolved(
            &facts,
            ("src/caller.rs", "run", "helper", ReferenceKind::Calls),
        );
    }
}

fn unresolved(facts: &CanonicalGenerationFacts, source: (&str, &str, &str, ReferenceKind)) {
    let (path, owner, name, kind) = source;
    let owner = capability_symbol(facts, path, owner);
    let reference = CapabilityReferenceQuery::new(facts, owner).named(name, kind);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

fn resolved(
    facts: &CanonicalGenerationFacts,
    source: (&str, &str, &str, ReferenceKind),
    target: (&str, &str, &str),
) {
    let (path, owner, name, kind) = source;
    let owner = capability_symbol(facts, path, owner);
    let reference = CapabilityReferenceQuery::new(facts, owner).named(name, kind);
    let symbol = capability_symbol(facts, target.0, target.1);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&symbol.symbol_id),
        "{reference:?}"
    );
    assert_eq!(reference.resolution_provenance, target.2);
    assert_eq!(reference.confidence, 1.0);
}
