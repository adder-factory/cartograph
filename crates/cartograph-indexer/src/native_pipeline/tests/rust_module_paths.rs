use super::unqual_resolution::generation;
use super::{
    CapabilityReferenceQuery, ReferenceKind, build_capability_generation, capability_symbol,
};

const CARGO: &str = "[package]\nname = \"module-paths\"\nversion = \"0.1.0\"\n";

#[test]
fn rust_module_paths_honor_internal_file_modules_from_the_parent_subtree() {
    for visibility in ["pub(super)", "pub(crate)", ""] {
        let declaration = format!("{visibility} mod exposed;");
        let facts = generation(&[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod outer; pub fn outside() { outer::facade::exposed::helper(); }",
            ),
            (
                "src/outer.rs",
                "pub mod facade; pub fn run() { facade::exposed::helper(); }",
            ),
            ("src/outer/facade.rs", &declaration),
            ("src/outer/facade/exposed.rs", "pub(crate) fn helper() {}"),
        ]);
        let owner = capability_symbol(&facts, "src/outer.rs", "run");
        let target = capability_symbol(&facts, "src/outer/facade/exposed.rs", "helper");
        let call = CapabilityReferenceQuery::new(&facts, owner)
            .named("facade::exposed::helper", ReferenceKind::Calls);
        assert_eq!(
            call.target_symbol_id.as_ref(),
            (!visibility.is_empty()).then_some(&target.symbol_id),
            "{visibility}: {call:?}"
        );
        let outside = capability_symbol(&facts, "src/lib.rs", "outside");
        let call = CapabilityReferenceQuery::new(&facts, outside)
            .named("outer::facade::exposed::helper", ReferenceKind::Calls);
        assert_eq!(
            call.target_symbol_id.as_ref(),
            (visibility == "pub(crate)").then_some(&target.symbol_id),
            "{visibility}: {call:?}"
        );
    }
}

#[test]
fn rust_crate_visibility_rejects_a_dependency_crate() {
    for (module, function, resolved) in [
        ("pub(crate)", "pub", false),
        ("pub", "pub(crate)", false),
        ("pub", "pub", true),
    ] {
        let declaration = format!("{module} mod exposed;");
        let helper = format!("{function} fn helper() {{}}");
        let facts = generation(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/app\", \"crates/tools\"]\n",
            ),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\ntools = { path = \"../tools\" }\n",
            ),
            (
                "crates/app/src/lib.rs",
                "pub fn run() { tools::exposed::helper(); }",
            ),
            (
                "crates/tools/Cargo.toml",
                "[package]\nname = \"tools\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/tools/src/lib.rs", &declaration),
            ("crates/tools/src/exposed.rs", &helper),
        ]);
        let owner = capability_symbol(&facts, "crates/app/src/lib.rs", "run");
        let target = capability_symbol(&facts, "crates/tools/src/exposed.rs", "helper");
        let call = CapabilityReferenceQuery::new(&facts, owner)
            .named("tools::exposed::helper", ReferenceKind::Calls);
        assert_eq!(
            call.target_symbol_id.as_ref(),
            resolved.then_some(&target.symbol_id),
            "{module} module, {function} function: {call:?}"
        );
    }
}

#[test]
fn rust_crate_visibility_keeps_owner_proofs_without_guessing_an_orphan_root() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        ("src/lib.rs", "pub fn root() {}"),
        (
            "src/orphan.rs",
            "mod outer { pub(crate) fn helper() {} mod nested { use super::*; pub fn run() { helper(); } } } pub fn outside() { self::outer::helper(); }",
        ),
        (
            "src/consumer.rs",
            "pub fn run() { super::orphan::outer::helper(); }",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/orphan.rs", "outer::nested::run");
    let target = capability_symbol(&facts, "src/orphan.rs", "outer::helper");
    let call = CapabilityReferenceQuery::new(&facts, owner).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    for (file, owner, name) in [
        ("src/orphan.rs", "outside", "self::outer::helper"),
        ("src/consumer.rs", "run", "super::orphan::outer::helper"),
    ] {
        let owner = capability_symbol(&facts, file, owner);
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none(), "{call:?}");
    }
}

#[test]
fn rust_crate_visibility_preserves_cancellation_outside_the_immediate_owner() {
    let (index, span) = visibility_resolution_index(
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", "pub mod api; pub fn run() { api::helper(); }"),
            ("src/api.rs", "pub(crate) fn helper() {}"),
        ],
        ("src/lib.rs", "api::helper"),
    );
    let request = visibility_request(&index, ("src/lib.rs", span, "api::helper"));
    let target_file = index
        .modules
        .files
        .iter()
        .find(|(_, file)| file.path == "src/api.rs")
        .map_or_else(|| panic!("missing fixture target"), |(id, _)| id);
    let target = &super::resolution_candidates_for_file(&index, "helper", target_file)[0];
    let scope = super::super::rust_inline_modules::ModuleScope {
        file: target_file,
        inline: "",
        module: None,
    };
    assert!(super::super::rust_path_visibility::crate_visible(
        &index,
        (&request, scope)
    ));
    assert!(
        super::super::rust_path_visibility::declaration_visible(
            &index,
            (&request, scope, target.visibility),
            &mut || false,
        )
        .unwrap_or_else(|_| panic!("original visibility proof failed"))
    );
    let mut polls = 0;
    let result = super::super::rust_path_visibility::visible(
        &index,
        (&request, scope, "helper", target),
        &mut || {
            polls += 1;
            true
        },
    );
    assert!(
        result.is_err(),
        "cancelled visibility must fail before a crate allowance"
    );
    assert_eq!(polls, 1);
}

#[test]
fn rust_file_module_visibility_preserves_existing_filesystem_cancellation_polls() {
    let mut private_polls = 0;
    for visibility in ["", "pub(super)", "pub(crate)", "pub"] {
        let declaration = format!("{visibility} mod api; pub fn run() {{ api::helper(); }}");
        let (index, span) = visibility_resolution_index(
            &[
                ("Cargo.toml", CARGO),
                ("src/lib.rs", "pub mod outer;"),
                ("src/outer.rs", &declaration),
                ("src/outer/api.rs", "pub fn helper() {}"),
            ],
            ("src/outer.rs", "api::helper"),
        );
        let request = visibility_request(&index, ("src/outer.rs", span, "api::helper"));
        let scope = super::super::rust_inline_modules::ModuleScope {
            file: request.file_id,
            inline: "",
            module: None,
        };
        let expected = index
            .candidates
            .get("helper")
            .and_then(|bucket| bucket.candidates.first())
            .unwrap_or_else(|| panic!("missing fixture target"));
        let mut polls = 0;
        let target = super::super::rust_path_resolution::resolve_in_scope(
            &index,
            (&request, scope, request.name),
            &mut || {
                polls += 1;
                false
            },
        )
        .unwrap_or_else(|_| panic!("non-cancelled {visibility} visibility failed"));
        assert_eq!(
            target.as_ref().map(|target| &target.symbol_id),
            Some(&expected.symbol_id)
        );
        if visibility.is_empty() {
            private_polls = polls;
        } else {
            assert_eq!(
                polls, private_polls,
                "{visibility} added cancellation polls"
            );
        }
        let mut polls = 0;
        let target = super::super::rust_path_resolution::resolve_in_scope(
            &index,
            (&request, scope, request.name),
            &mut || {
                polls += 1;
                polls > private_polls
            },
        )
        .unwrap_or_else(|_| panic!("{visibility} cancelled after the original success"));
        assert_eq!(
            target.as_ref().map(|target| &target.symbol_id),
            Some(&expected.symbol_id)
        );
        assert_eq!(polls, private_polls);
    }
}

fn visibility_request<'a>(
    index: &'a super::ResolutionIndex,
    (file_path, span, name): (&'a str, super::SourceSpan, &'a str),
) -> super::ResolutionRequest<'a> {
    let file_id = index
        .modules
        .files
        .iter()
        .find(|(_, file)| file.path == file_path)
        .map_or_else(|| panic!("missing fixture file {file_path}"), |(id, _)| id);
    let owner = &super::resolution_candidates_for_file(index, "run", file_id)[0];
    super::ResolutionRequest {
        file_id,
        file_path,
        language: "rust",
        import_bindings: super::ImportBindingSelection::empty(),
        owner: Some(&owner.symbol_id),
        name,
        dispatch: super::ReferenceDispatch::Static,
        kind: ReferenceKind::Calls,
        span,
    }
}

fn visibility_resolution_index(
    files: &[(&str, &str)],
    (reference_path, reference_name): (&str, &str),
) -> (super::ResolutionIndex, super::SourceSpan) {
    let limits = super::SourceLimits::new(super::TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("invalid fixture limits: {error}"));
    let mut accumulator = super::NativeFactAccumulator::new(super::TEST_GENERATION_BYTES);
    let mut span = None;
    for &(path, source) in files {
        let snapshot =
            cartograph_extract::SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
                .unwrap_or_else(|error| panic!("invalid fixture snapshot: {error}"));
        let mut extractor = super::NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("fixture grammar missing: {error}"));
        let file = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("fixture extraction failed: {error}"));
        if path == reference_path {
            span = file
                .references
                .iter()
                .find(|reference| reference.name == reference_name)
                .map(|reference| reference.span);
        }
        accumulator
            .push(file)
            .unwrap_or_else(|_| panic!("fixture exceeded its budget"));
    }
    (
        super::mybatis_test_resolution_index(&accumulator, super::TEST_GENERATION_BYTES),
        span.unwrap_or_else(|| panic!("missing fixture call")),
    )
}

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
