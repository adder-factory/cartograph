use super::unqual_resolution::generation;
use super::{
    CapabilityReferenceQuery, ReferenceKind, build_capability_generation, capability_symbol,
};

const CARGO: &str = "[package]\nname = \"module-paths\"\nversion = \"0.1.0\"\n";

#[test]
fn rust_module_self_paths_use_the_enclosing_inline_module() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "pub fn helper() {} pub mod nested { pub fn helper() {} pub fn run() { self::helper(); crate::helper(); } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/lib.rs", "nested::run");
    for (name, target) in [
        ("self::helper", "nested::helper"),
        ("crate::helper", "helper"),
    ] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        let target = capability_symbol(&facts, "src/lib.rs", target);
        assert_eq!(
            call.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{name}"
        );
        assert_eq!(call.resolution_provenance, "native-rust-qualified-path");
        assert_eq!(call.confidence, 1.0);
    }
}

#[test]
fn rust_module_self_paths_can_access_a_private_helper_in_their_own_module() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "pub fn helper() {} pub mod nested { fn helper() {} pub fn run() { self::helper(); } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/lib.rs", "nested::run");
    let target = capability_symbol(&facts, "src/lib.rs", "nested::helper");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("self::helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-rust-qualified-path");
    assert_eq!(call.confidence, 1.0);
}

#[test]
fn rust_module_paths_do_not_merge_distinct_inline_module_declarations() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "#[cfg(feature = \"first\")] pub mod nested { pub fn helper() {} } #[cfg(feature = \"second\")] pub mod nested { pub fn run() { self::helper(); } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/lib.rs", "nested::run");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("self::helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn rust_module_super_paths_pop_inline_modules_before_physical_modules() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        ("src/lib.rs", "pub fn helper() {} pub mod outer;"),
        (
            "src/outer.rs",
            "pub fn helper() {} pub mod nested { fn helper() {} pub mod inner { pub fn run() { super::helper(); super::super::helper(); super::super::super::helper(); } } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/outer.rs", "nested::inner::run");
    for (name, file, target) in [
        ("super::helper", "src/outer.rs", "nested::helper"),
        ("super::super::helper", "src/outer.rs", "helper"),
        ("super::super::super::helper", "src/lib.rs", "helper"),
    ] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        let target = capability_symbol(&facts, file, target);
        assert_eq!(
            call.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{name}"
        );
        assert_eq!(call.resolution_provenance, "native-rust-qualified-path");
        assert_eq!(call.confidence, 1.0);
    }
}

#[test]
fn rust_module_paths_do_not_borrow_an_impl_type_declaration_scope() {
    let facts = build_capability_generation(
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod nested { pub fn helper() {} pub struct Worker<T>(pub T); } mod outside;",
            ),
            (
                "src/outside.rs",
                "use crate::nested::Worker; impl<T> Worker<T> { pub fn run() { self::helper(); } }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(&facts, "src/outside.rs", "Worker::run");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("self::helper", ReferenceKind::Calls);
    assert!(call.target_symbol_id.is_none(), "{call:?}");
}

#[test]
fn rust_module_paths_preserve_base_resolution_when_the_impl_scope_is_unproven() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "pub mod nested { pub fn helper() {} pub struct Worker<T>(pub T); } mod outside;",
        ),
        (
            "src/outside.rs",
            "use crate::nested::Worker; pub fn helper() {} impl<T> Worker<T> { pub fn run() { self::helper(); } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/outside.rs", "Worker::run");
    let target = capability_symbol(&facts, "src/outside.rs", "helper");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("self::helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-rust-qualified-path");
    assert_eq!(call.confidence, 1.0);
}

#[test]
fn rust_module_paths_keep_the_verified_root_impl_anchor() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        ("src/lib.rs", "pub fn helper() {} mod outside;"),
        (
            "src/outside.rs",
            "use super::helper; pub struct Worker; impl Worker { pub fn run() { helper(); super::helper(); } }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/outside.rs", "Worker::run");
    let target = capability_symbol(&facts, "src/lib.rs", "helper");
    for (name, provenance) in [
        ("helper", "native-import-binding"),
        ("super::helper", "native-rust-qualified-path"),
    ] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(call.resolution_provenance, provenance);
        assert_eq!(call.confidence, 1.0);
    }
}

#[test]
fn rust_module_paths_do_not_cross_private_inline_children_from_the_file_root() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "pub mod outer { pub mod exposed { pub fn helper() {} } mod hidden { pub fn helper() {} pub fn inside() { self::helper(); } } } pub fn run() { self::outer::hidden::helper(); crate::outer::hidden::helper(); self::outer::exposed::helper(); }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/lib.rs", "run");
    for name in [
        "self::outer::hidden::helper",
        "crate::outer::hidden::helper",
    ] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
    let target = capability_symbol(&facts, "src/lib.rs", "outer::exposed::helper");
    let call = CapabilityReferenceQuery::new(&facts, owner)
        .named("self::outer::exposed::helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-rust-qualified-path");
    assert_eq!(call.confidence, 1.0);
    let owner = capability_symbol(&facts, "src/lib.rs", "outer::hidden::inside");
    let target = capability_symbol(&facts, "src/lib.rs", "outer::hidden::helper");
    let call =
        CapabilityReferenceQuery::new(&facts, owner).named("self::helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-rust-qualified-path");
    assert_eq!(call.confidence, 1.0);
}
