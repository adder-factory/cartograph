use super::{
    CapabilityReferenceQuery, EdgeKind, ReferenceKind, build_capability_generation,
    capability_symbol,
};

#[test]
fn rust_self_inline_nominals_do_not_fall_through_to_competing_module_files() {
    for receiver in ["Other", "self::Other"] {
        let source = format!(
            "mod nested {{ struct Other; impl {receiver} {{ fn call(&self) {{ self.scan(); }} }} }}"
        );
        let generation = build_capability_generation(
            &[
                ("src/lib.rs", "mod outer;"),
                ("src/outer/mod.rs", &source),
                (
                    "src/outer/nested.rs",
                    "pub struct Other; impl Other { pub fn scan(&self) {} }",
                ),
            ],
            false,
        );
        let caller = generation
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name.ends_with("::call"))
            .unwrap_or_else(|| panic!("missing inline receiver caller"));
        let reference = CapabilityReferenceQuery::new(&generation, caller)
            .named("self.scan", ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{receiver}");
    }
}

#[test]
fn rust_self_function_local_modules_do_not_guess_file_root_owners() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "mod nested { struct Local; impl Local { fn scan(&self) {} } } fn scope() { mod nested { struct Local; impl Local { fn call(&self) { self.scan(); } } } }",
        )],
        false,
    );
    let caller = generation
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name.ends_with("::call"))
        .unwrap_or_else(|| panic!("missing function-local module caller"));
    let reference =
        CapabilityReferenceQuery::new(&generation, caller).named("self.scan", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn rust_self_relative_inline_paths_keep_their_module_anchor() {
    for binding in [
        "use self::Other as Runtime; impl Runtime",
        "impl self::Other",
        "use super::nested::Other as Runtime; impl Runtime",
        "impl super::nested::Other",
    ] {
        for method in ["fn scan(&self) {}", "fn different(&self) {}"] {
            let source = format!(
                "struct Other; impl Other {{ fn scan(&self) {{}} }} mod nested {{ struct Other; impl Other {{ {method} }} {binding} {{ fn call(&self) {{ self.scan(); }} }} }}"
            );
            let generation = build_capability_generation(
                &[("src/lib.rs", "mod outer;"), ("src/outer/mod.rs", &source)],
                false,
            );
            let caller = generation
                .symbols()
                .iter()
                .find(|symbol| symbol.qualified_name.ends_with("::call"))
                .unwrap_or_else(|| panic!("missing relative receiver caller: {binding}"));
            let reference = CapabilityReferenceQuery::new(&generation, caller)
                .named("self.scan", ReferenceKind::Calls);
            if method.starts_with("fn scan") {
                let target =
                    capability_symbol(&generation, "src/outer/mod.rs", "nested::Other::scan");
                assert_eq!(
                    reference.target_symbol_id.as_ref(),
                    Some(&target.symbol_id),
                    "{binding}"
                );
            } else {
                assert!(reference.target_symbol_id.is_none(), "{binding}");
            }
        }
    }
}

#[test]
fn rust_self_block_local_types_do_not_guess_file_root_owners() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "struct Local; impl Local { fn scan(&self) {} } fn scope() { struct Local; impl Local { fn call(&self) { self.scan(); } } }",
        )],
        false,
    );
    let caller = generation
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name.ends_with("::call"))
        .unwrap_or_else(|| panic!("missing block-local caller"));
    let reference =
        CapabilityReferenceQuery::new(&generation, caller).named("self.scan", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn rust_self_blanket_impl_type_parameter_is_not_a_same_named_concrete_type() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "struct T; impl T { fn target(&self) {} } trait Target { fn target(&self); } trait Caller { fn call(&self); } impl<T: Target> Caller for T { fn call(&self) { self.target(); } }",
        )],
        false,
    );
    let caller = capability_symbol(&generation, "src/lib.rs", "T::call");
    let reference = CapabilityReferenceQuery::new(&generation, caller)
        .named("self.target", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn rust_self_inline_import_alias_overrides_root_type_without_unrelated_fallback() {
    for method in ["fn scan(&self) {}", "fn different(&self) {}"] {
        let source = format!(
            "struct Runtime; impl Runtime {{ fn scan(&self) {{}} fn root_call(&self) {{ self.scan(); }} }} struct Other; impl Other {{ {method} }} mod nested {{ use crate::{{Other as Runtime}}; impl Runtime {{ fn call(&self) {{ self.scan(); }} }} }}"
        );
        let generation = build_capability_generation(&[("src/lib.rs", &source)], false);
        let caller = capability_symbol(&generation, "src/lib.rs", "nested::Runtime::call");
        let reference = CapabilityReferenceQuery::new(&generation, caller)
            .named("self.scan", ReferenceKind::Calls);
        if method.starts_with("fn scan") {
            let target = capability_symbol(&generation, "src/lib.rs", "Other::scan");
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        } else {
            assert!(reference.target_symbol_id.is_none());
        }
        let root_call = capability_symbol(&generation, "src/lib.rs", "Runtime::root_call");
        let root_target = capability_symbol(&generation, "src/lib.rs", "Runtime::scan");
        let root_reference = CapabilityReferenceQuery::new(&generation, root_call)
            .named("self.scan", ReferenceKind::Calls);
        assert_eq!(
            root_reference.target_symbol_id.as_ref(),
            Some(&root_target.symbol_id)
        );
    }
}

#[test]
fn rust_self_calls_resolve_private_parent_methods_across_impl_files() {
    let fixtures = [
        (
            "src/lib.rs",
            "mod source_context;\npub struct Runtime;\nimpl Runtime { fn scan_source(&self) -> u8 { 1 } }\npub struct Other;\nimpl Other { pub fn scan_source(&self) -> u8 { 2 } }\n",
        ),
        (
            "src/source_context.rs",
            "use crate::Runtime;\nimpl Runtime { pub fn symbol_context(&self) -> u8 { self.scan_source() } pub fn file_context(&self) -> u8 { self.scan_source() } }\n",
        ),
    ];
    let forward = build_capability_generation(&fixtures, false);
    let reversed = build_capability_generation(&fixtures, true);
    assert_eq!(forward.digest(), reversed.digest());
    let target = capability_symbol(&forward, "src/lib.rs", "Runtime::scan_source");
    for caller_name in ["Runtime::symbol_context", "Runtime::file_context"] {
        let caller = capability_symbol(&forward, "src/source_context.rs", caller_name);
        let reference = CapabilityReferenceQuery::new(&forward, caller)
            .named("self.scan_source", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == caller.symbol_id
                && edge.target_symbol_id == target.symbol_id
                && edge.kind == EdgeKind::Calls
                && edge.site_count == 1
        }));
    }
}

#[test]
fn rust_self_calls_resolve_import_aliases_and_generic_receivers() {
    let generation = build_capability_generation(
        &[
            (
                "src/lib.rs",
                "mod child; pub struct Runtime<T>(T); impl<T> Runtime<T> { fn scan(&self) {} }",
            ),
            (
                "src/child.rs",
                "use crate::Runtime as Alias; impl<T> Alias<T> { fn call(&self) { self.scan(); } }",
            ),
        ],
        false,
    );
    let caller = capability_symbol(&generation, "src/child.rs", "Alias::call");
    let target = capability_symbol(&generation, "src/lib.rs", "Runtime::scan");
    let reference =
        CapabilityReferenceQuery::new(&generation, caller).named("self.scan", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
}

#[test]
fn rust_self_calls_do_not_guess_unrelated_or_inaccessible_methods() {
    let generation = build_capability_generation(
        &[
            (
                "src/lib.rs",
                "mod a; mod b; pub struct Other; impl Other { pub fn missing(&self) {} } pub struct Runtime;",
            ),
            (
                "src/a.rs",
                "pub struct Hidden; impl Hidden { fn secret(&self) {} }",
            ),
            (
                "src/b.rs",
                "use crate::a::Hidden; use crate::Runtime; impl Hidden { fn call(&self) { self.secret(); } } impl Runtime { fn call(&self) { self.missing(); } } trait Unknown { fn call(&self) { self.missing(); } }",
            ),
        ],
        false,
    );
    for (caller_name, name) in [
        ("Hidden::call", "self.secret"),
        ("Runtime::call", "self.missing"),
        ("Unknown::call", "self.missing"),
    ] {
        let caller = capability_symbol(&generation, "src/b.rs", caller_name);
        let reference =
            CapabilityReferenceQuery::new(&generation, caller).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{caller_name}");
    }
}

#[test]
fn rust_self_calls_preserve_ambiguity_across_trait_implementations() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "struct Runtime; trait A { fn run(&self); } trait B { fn run(&self); } impl A for Runtime { fn run(&self) {} } impl B for Runtime { fn run(&self) {} } impl Runtime { fn call(&self) { self.run(); } }",
        )],
        false,
    );
    let caller = capability_symbol(&generation, "src/lib.rs", "Runtime::call");
    let reference =
        CapabilityReferenceQuery::new(&generation, caller).named("self.run", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
}

#[test]
fn rust_self_calls_resolve_recursive_methods() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "struct Runtime; impl Runtime { fn run(&self) { self.run(); } }",
        )],
        false,
    );
    let caller = capability_symbol(&generation, "src/lib.rs", "Runtime::run");
    let reference =
        CapabilityReferenceQuery::new(&generation, caller).named("self.run", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&caller.symbol_id));
}

#[test]
fn rust_self_calls_in_inline_modules_keep_their_lexical_owner() {
    let generation = build_capability_generation(
        &[(
            "src/lib.rs",
            "mod nested { pub struct Runtime; impl Runtime { fn scan(&self) {} fn call(&self) { self.scan(); } } } struct Runtime; impl Runtime { fn scan(&self) {} }",
        )],
        false,
    );
    let caller = capability_symbol(&generation, "src/lib.rs", "nested::Runtime::call");
    let target = capability_symbol(&generation, "src/lib.rs", "nested::Runtime::scan");
    let reference =
        CapabilityReferenceQuery::new(&generation, caller).named("self.scan", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
}
