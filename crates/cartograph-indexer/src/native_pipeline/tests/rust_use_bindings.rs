use super::{
    CapabilityReferenceQuery, ReferenceKind, build_capability_generation, capability_symbol,
};

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
