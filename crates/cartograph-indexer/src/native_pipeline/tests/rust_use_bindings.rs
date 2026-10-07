use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, ReferenceKind, build_capability_generation,
    capability_symbol,
};

#[test]
fn rust_standard_statement_macros_keep_module_use_bindings() {
    for statement in [
        "assert!(x);",
        "std::assert!(x);",
        "core::assert!(x);",
        "std :: assert!(x);",
        "core /* layout */ :: assert!(x);",
        "println!(\"{}\", x);",
        "dbg!(x);",
        "vec![x];",
    ] {
        let source =
            format!("use super::alpha; pub fn run(x: bool) {{ {statement} alpha::resolve(); }}");
        let facts = macro_use_generation(&source);
        assert_macro_use_target(&facts, ("run", "alpha"));
    }
}

#[test]
fn rust_statement_macros_fence_only_their_block_before_and_after() {
    for invocation in ["introduce!();", "introduce! {}"] {
        let source = format!(
            "use super::{{alpha, beta, gamma}}; pub fn run() {{ {{ alpha::resolve(); {invocation} beta::resolve(); }} gamma::resolve(); }} pub fn sibling() {{ alpha::resolve(); }}"
        );
        let facts = macro_use_generation(&source);
        for module in ["alpha", "beta"] {
            let owner = capability_symbol(&facts, "src/consumer.rs", "run");
            let reference = CapabilityReferenceQuery::new(&facts, owner)
                .named(&format!("{module}::resolve"), ReferenceKind::Calls);
            assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        }
        assert_macro_use_target(&facts, ("run", "gamma"));
        assert_macro_use_target(&facts, ("sibling", "alpha"));
    }
}

#[test]
fn rust_expression_macros_do_not_fence_surrounding_use_bindings() {
    for expression in [
        "let _ = introduce!();",
        "consume(introduce!());",
        "(introduce!());",
    ] {
        let source = format!("use super::alpha; pub fn run() {{ {expression} alpha::resolve(); }}");
        let facts = macro_use_generation(&source);
        assert_macro_use_target(&facts, ("run", "alpha"));
    }
}

#[test]
fn rust_outer_statement_macro_fences_survive_nested_block_intervals() {
    let facts = macro_use_generation(
        "use super::{alpha, beta}; pub fn run() { introduce!(); { another!(); } alpha::resolve(); } pub fn sibling() { beta::resolve(); }",
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("alpha::resolve", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
    assert_macro_use_target(&facts, ("sibling", "beta"));
}

#[test]
fn rust_foreign_assert_macro_is_not_a_known_standard_expression() {
    let facts = macro_use_generation(
        "use super::alpha; pub fn run(x: bool) { foreign::assert!(x); alpha::resolve(); }",
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "run");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("alpha::resolve", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none(), "{reference:?}");
}

#[test]
fn rust_custom_standard_macro_names_keep_block_fences() {
    for source in [
        "use super::alpha; macro_rules! assert { () => { mod alpha { pub fn resolve() {} } } } pub fn run() { assert!(); alpha::resolve(); } pub fn sibling() { alpha::resolve(); }",
        "use super::alpha; pub fn run() { macro_rules! matches { () => { mod alpha { pub fn resolve() {} } } } matches!(); alpha::resolve(); } pub fn sibling() { alpha::resolve(); }",
    ] {
        let facts = macro_use_generation(source);
        let owner = capability_symbol(&facts, "src/consumer.rs", "run");
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("alpha::resolve", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{reference:?}");
        assert_macro_use_target(&facts, ("sibling", "alpha"));
    }
}

#[test]
fn rust_expired_custom_macro_names_do_not_fence_standard_statements() {
    let facts = macro_use_generation(
        "use super::alpha; pub fn before() { macro_rules! assert { () => { mod alpha { pub fn resolve() {} } } } } pub fn run(x: bool) { assert!(x); alpha::resolve(); }",
    );
    assert_macro_use_target(&facts, ("run", "alpha"));
}

#[test]
fn rust_imported_macro_and_namespace_overrides_fence_standard_spellings() {
    for source in [
        "use super::alpha; use crate::custom_macros::assert; pub fn run() { assert!(); alpha::resolve(); }",
        "use super::alpha; pub fn run() { assert!(); alpha::resolve(); } use crate::custom_assert as assert;",
        "use super::alpha; use crate::custom_macros as std; pub fn run() { std::assert!(); alpha::resolve(); }",
        "use super::alpha; pub fn run() { core::assert!(); alpha::resolve(); } use crate::custom_macros as core;",
        "use super::alpha; pub fn run() { assert!(); alpha::resolve(); } use crate::custom_macros::*;",
        "use super::alpha; pub fn run() { assert!(); alpha::resolve(); } use std::include as assert;",
        "use super::alpha; use crate::custom_macros as std; use std::assert; pub fn run() { assert!(); alpha::resolve(); }",
        "use super::alpha; use core::assert; pub fn run() { assert!(); alpha::resolve(); } use crate::custom_macros as core;",
        "use super::alpha; mod std { pub use crate::custom_assert as assert; } pub fn run() { std::assert!(); alpha::resolve(); }",
        "use super::alpha; pub fn run() { core::assert!(); alpha::resolve(); } mod core { pub use crate::custom_assert as assert; }",
    ] {
        let facts = macro_use_generation(source);
        let owner = capability_symbol(&facts, "src/consumer.rs", "run");
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("alpha::resolve", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{source}: {reference:?}"
        );
    }
}

#[test]
fn rust_known_standard_imports_keep_statement_macro_exemptions() {
    for import in ["use std::assert;", "use core::assert;", "use std::*;"] {
        let source = format!(
            "use super::alpha; pub fn run(x: bool) {{ assert!(x); alpha::resolve(); }} {import}"
        );
        let facts = macro_use_generation(&source);
        assert_macro_use_target(&facts, ("run", "alpha"));
    }
}

#[test]
fn rust_macro_use_imports_withdraw_only_later_bare_macro_exemptions() {
    for source in [
        "use super::alpha; #[macro_use] mod macros; pub fn run() { assert!(); alpha::resolve(); }",
        "use super::alpha; pub fn run() { #[path = \"consumer/macros.rs\"] #[macro_use] mod macros; assert!(); alpha::resolve(); }",
    ] {
        let facts = macro_use_generation(source);
        let owner = capability_symbol(&facts, "src/consumer.rs", "run");
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("alpha::resolve", ReferenceKind::Calls);
        assert!(
            reference.target_symbol_id.is_none(),
            "{source}: {reference:?}"
        );
    }
    for source in [
        "use super::alpha; #[macro_use] mod macros; pub fn run(x: bool) { std::assert!(x); alpha::resolve(); }",
        "use super::alpha; pub fn run(x: bool) { assert!(x); alpha::resolve(); } #[macro_use] mod macros;",
        "use super::alpha; pub fn run(x: bool) { assert!(x); #[macro_use] mod macros { macro_rules! assert { () => { mod alpha { pub fn resolve() {} } } } } alpha::resolve(); }",
    ] {
        let facts = macro_use_generation(source);
        assert_macro_use_target(&facts, ("run", "alpha"));
    }
}

fn macro_use_generation(source: &str) -> CanonicalGenerationFacts {
    build_capability_generation(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "src/lib.rs",
                "mod alpha; mod beta; mod gamma; mod custom_macros; mod consumer;",
            ),
            ("src/alpha.rs", "pub fn resolve() {}"),
            ("src/beta.rs", "pub fn resolve() {}"),
            ("src/gamma.rs", "pub fn resolve() {}"),
            (
                "src/custom_macros.rs",
                "#[macro_export] macro_rules! custom_assert { () => { mod alpha { pub fn resolve() {} } } } pub use crate::custom_assert as assert;",
            ),
            (
                "src/consumer/macros.rs",
                "macro_rules! assert { () => { mod alpha { pub fn resolve() {} } } }",
            ),
            ("src/consumer.rs", source),
        ],
        false,
    )
}

fn assert_macro_use_target(facts: &CanonicalGenerationFacts, query: (&str, &str)) {
    let (owner, module) = query;
    let owner = capability_symbol(facts, "src/consumer.rs", owner);
    let reference = CapabilityReferenceQuery::new(facts, owner)
        .named(&format!("{module}::resolve"), ReferenceKind::Calls);
    let target = capability_symbol(facts, &format!("src/{module}.rs"), "resolve");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, "native-import-binding");
    assert_eq!(reference.confidence, 1.0);
}

#[test]
fn rust_use_calls_require_unshadowed_file_module_bindings() {
    let facts = build_capability_generation(
        &[
            ("src/lib.rs", "mod helpers; mod decoy; mod consumer;"),
            ("src/helpers.rs", "pub fn helper() {}"),
            ("src/decoy.rs", "pub fn helper() {}"),
            (
                "src/consumer.rs",
                "use crate::helpers::helper; pub fn use_it() { helper(); } pub fn generic<helper>() { helper(); } pub fn parameter(helper: fn()) { helper(); } pub fn nearer() { fn helper() {} helper(); }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    let target = capability_symbol(&facts, "src/helpers.rs", "helper");
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-import-binding");
    assert_eq!(call.confidence, 1.0);
    let owner = capability_symbol(&facts, "src/consumer.rs", "generic");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    let owner = capability_symbol(&facts, "src/consumer.rs", "parameter");
    let target = capability_symbol(&facts, "src/consumer.rs", "parameter::helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-exact-lexical");
    let owner = capability_symbol(&facts, "src/consumer.rs", "nearer");
    let target = capability_symbol(&facts, "src/consumer.rs", "nearer::helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-exact-lexical");
    assert_eq!(call.confidence, 1.0);
}

#[test]
fn rust_use_types_in_plain_impls_reject_generic_impl_shadowing() {
    let facts = build_capability_generation(
        &[
            (
                "src/lib.rs",
                "mod models; mod decoy; use crate::models::Slot; struct Store; struct Generic; trait Convert<T> { fn create(value: T); } impl Store { pub fn create(value: Slot) {} } impl<Slot> Convert<Slot> for Generic { fn create(value: Slot) {} }",
            ),
            ("src/models.rs", "pub struct Slot {}"),
            ("src/decoy.rs", "pub struct Slot {}"),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/lib.rs", "Store::create");
    let target = capability_symbol(&facts, "src/models.rs", "Slot");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("Slot", ReferenceKind::TypeOf);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-import-binding");
    assert_eq!(call.confidence, 1.0);
    let owner = capability_symbol(&facts, "src/lib.rs", "Generic::create");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("Slot", ReferenceKind::TypeOf);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn rust_restricted_visibility_does_not_hide_a_plain_callable_header() {
    let facts = build_capability_generation(
        &[
            ("src/lib.rs", "mod consumer; fn helper() {}"),
            (
                "src/consumer.rs",
                "use crate::helper; pub(super) fn use_it() { helper(); }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
    let target = capability_symbol(&facts, "src/lib.rs", "helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-import-binding");
    assert_eq!(call.confidence, 1.0);
}

#[test]
fn rust_nested_use_does_not_bind_a_sibling_module_call() {
    let facts = build_capability_generation(
        &[
            (
                "src/lib.rs",
                "mod helpers; mod decoy; mod nested { use crate::helpers::helper; } pub fn use_it() { helper(); }",
            ),
            ("src/helpers.rs", "pub fn helper() {}"),
            ("src/decoy.rs", "pub fn helper() {}"),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/lib.rs", "use_it");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn rust_use_calls_abstain_for_unrepresented_local_patterns() {
    for source in [
        "pub fn use_it() { let helper = || {}; helper(); }",
        "pub fn use_it() { let mut helper = || {}; helper(); }",
        "pub fn use_it() { let (helper,) = (|| {},); helper(); }",
        "pub fn use_it() { for helper in [|| {}] { helper(); } }",
        "pub fn use_it() { if let Some(helper) = Some(|| {}) { helper(); } }",
        "pub fn use_it() { match Some(|| {}) { Some(helper) => helper(), _ => () } }",
        "pub fn use_it() { let _ = (|helper: fn()| helper())(|| {}); }",
        "pub fn use_it((helper,): (fn(),)) { helper(); }",
    ] {
        let source = format!("use crate::helpers::helper; {source}");
        let facts = build_capability_generation(
            &[
                ("src/lib.rs", "mod helpers; mod decoy; mod consumer;"),
                ("src/helpers.rs", "pub fn helper() {}"),
                ("src/decoy.rs", "pub fn helper() {}"),
                ("src/consumer.rs", &source),
            ],
            false,
        );
        let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{source}: {call:?}");
    }
}

#[test]
fn rust_use_calls_abstain_inside_opaque_macro_arguments() {
    let facts = build_capability_generation(
        &[
            ("src/lib.rs", "mod helpers; mod decoy; mod consumer;"),
            ("src/helpers.rs", "pub fn helper() -> bool { true }"),
            ("src/decoy.rs", "pub fn helper() -> bool { true }"),
            (
                "src/consumer.rs",
                "use crate::helpers::helper; pub fn use_it() { assert!({ let helper = || true; helper() }); } pub fn direct() { helper(); }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
    let owner = capability_symbol(&facts, "src/consumer.rs", "direct");
    let target = capability_symbol(&facts, "src/helpers.rs", "helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-import-binding");
    assert_eq!(call.confidence, 1.0);
}

#[test]
fn rust_use_calls_abstain_for_macro_introduced_items_and_locals() {
    for body in [
        "macro_rules! bind { ($name:ident) => { let $name = || true; } } bind!(helper); helper();",
        "macro_rules! bind { () => { fn helper() {} } } bind!(); helper();",
        "macro_rules! bind { () => { fn helper() {} } } helper(); bind!();",
    ] {
        let source = format!("use crate::helpers::helper; pub fn use_it() {{ {body} }}");
        let facts = build_capability_generation(
            &[
                ("src/lib.rs", "mod helpers; mod decoy; mod consumer;"),
                ("src/helpers.rs", "pub fn helper() {}"),
                ("src/decoy.rs", "pub fn helper() {}"),
                ("src/consumer.rs", &source),
            ],
            false,
        );
        let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{body}: {call:?}");
    }
}

#[test]
fn rust_use_types_abstain_for_macro_introduced_type_items() {
    let facts = build_capability_generation(
        &[
            ("src/lib.rs", "mod models; mod decoy; mod consumer;"),
            ("src/models.rs", "pub struct Slot {}"),
            ("src/decoy.rs", "pub struct Slot {}"),
            (
                "src/consumer.rs",
                "use crate::models::Slot; pub fn use_it() { macro_rules! bind { () => { struct Slot {} } } bind!(); let value = Slot {}; }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("Slot", ReferenceKind::Instantiates);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn rust_root_macro_expansions_withdraw_import_proof() {
    let facts = build_capability_generation(
        &[
            ("src/lib.rs", "mod models; mod decoy; mod consumer;"),
            ("src/models.rs", "pub type helper = ();"),
            ("src/decoy.rs", "pub type helper = ();"),
            (
                "src/consumer.rs",
                "use crate::models::helper; macro_rules! make { ($name:ident) => { fn $name() {} } } make!(helper); pub fn use_it() { helper(); }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn rust_local_uncertainty_keeps_unshadowed_names_and_base_fallback() {
    for (body, decoy, provenance, confidence) in [
        (
            "let ignored = (); helper();",
            true,
            "native-import-binding",
            1.0,
        ),
        (
            "let helper = || {}; helper();",
            false,
            "native-exact-project",
            0.95,
        ),
    ] {
        let source = format!("use crate::helpers::helper; pub fn use_it() {{ {body} }}");
        let mut fixture = vec![
            ("src/lib.rs", "mod helpers; mod decoy; mod consumer;"),
            ("src/helpers.rs", "pub fn helper() {}"),
            ("src/consumer.rs", source.as_str()),
        ];
        if decoy {
            fixture.push(("src/decoy.rs", "pub fn helper() {}"));
        }
        let facts = build_capability_generation(&fixture, false);
        let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
        let target = capability_symbol(&facts, "src/helpers.rs", "helper");
        let call =
            CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
        assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(call.resolution_provenance, provenance);
        assert_eq!(call.confidence, confidence);
        if !decoy {
            let base = super::generic_repair::build_generation(
                super::generic_repair::CapabilityGenerationRequest {
                    fixtures: &fixture,
                    reverse: false,
                    wider_partial_band: false,
                    maximum_bytes: super::TEST_GENERATION_BYTES,
                },
                |file| {
                    file.import_bindings
                        .retain(|binding| binding.module_specifier != "crate::helpers::helper");
                },
                || false,
            );
            super::generic_repair::assert_base_reference(&base, call);
        }
    }
}

#[test]
fn rust_pattern_bindings_never_shadow_module_path_heads() {
    let facts = build_capability_generation(
        &[
            ("src/lib.rs", "mod helpers; mod consumer;"),
            ("src/helpers.rs", "pub fn helper() -> Option<u8> { None }"),
            (
                "src/consumer.rs",
                "use crate::helpers; pub fn use_it(values: &[(u8, u8)]) { for (left, _) in values { if let Some(value) = helpers::helper() { let _ = (left, value); } } }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/consumer.rs", "use_it");
    let target = capability_symbol(&facts, "src/helpers.rs", "helper");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("helpers::helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
}
