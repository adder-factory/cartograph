use super::{
    CapabilityReferenceQuery, ReferenceKind, build_capability_generation, capability_symbol,
};

#[test]
fn kamailio_route_resolution_requires_local_or_literal_include_identity() {
    let fixtures = [
        (
            "kamailio/kamailio.cfg",
            "#!KAMAILIO\ninclude_file \"auth.cfg\"\nrequest_route { route(AUTH); route(MISSING); t_on_failure(\"RETRY\"); t_relay(); }\nfailure_route[RETRY] {}\n",
        ),
        ("kamailio/auth.cfg", "route[AUTH] {}\n"),
        (
            "other/kamailio/unrelated.cfg",
            "route[AUTH] {}\nroute[MISSING] {}\nroute[t_relay] {}\n",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let reversed = build_capability_generation(&fixtures, true);
    assert_eq!(facts.digest(), reversed.digest());
    let owner = capability_symbol(&facts, "kamailio/kamailio.cfg", "kamailio::request_route");
    let auth = capability_symbol(&facts, "kamailio/auth.cfg", "kamailio::route::AUTH");
    let retry = capability_symbol(
        &facts,
        "kamailio/kamailio.cfg",
        "kamailio::failure_route::RETRY",
    );
    let call = CapabilityReferenceQuery::new(&facts, owner).named("AUTH", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&auth.symbol_id));
    let callback =
        CapabilityReferenceQuery::new(&facts, owner).named("RETRY", ReferenceKind::References);
    assert_eq!(callback.target_symbol_id.as_ref(), Some(&retry.symbol_id));
    for name in ["MISSING", "t_relay"] {
        let call = CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(call.target_symbol_id.is_none());
        assert_eq!(call.resolution_provenance, "native-kamailio-unresolved");
    }
    let imported = super::capability_reference_in_file(&facts, "kamailio/kamailio.cfg", "auth.cfg");
    let file = super::capability_file_symbol(&facts, "kamailio/auth.cfg");
    assert_eq!(imported.target_symbol_id.as_ref(), Some(&file.symbol_id));
    assert_eq!(
        imported.resolution_provenance,
        "native-kamailio-static-config"
    );
}

#[test]
fn kamailio_duplicate_routes_and_missing_or_dynamic_includes_abstain() {
    let fixtures = [
        (
            "kamailio/main.cfg",
            "include_file \"a.cfg\"\ninclude_file \"b.cfg\"\ninclude_file \"missing.cfg\"\nrequest_route { route(AUTH); route($var(target)); }\n",
        ),
        ("kamailio/a.cfg", "route[AUTH] {}\n"),
        ("kamailio/b.cfg", "route[AUTH] {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    assert_eq!(
        facts.digest(),
        build_capability_generation(&fixtures, true).digest()
    );
    let owner = capability_symbol(&facts, "kamailio/main.cfg", "kamailio::request_route");
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("AUTH", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    assert!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("route", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
    assert!(
        facts
            .references()
            .iter()
            .filter(|reference| reference.reference_name == "missing.cfg")
            .all(|reference| reference.target_symbol_id.is_none())
    );
}

#[test]
fn freeswitch_applications_and_modules_never_bind_to_unrelated_jvm_symbols() {
    let facts = build_capability_generation(
        &[
            (
                "freeswitch/dialplan/default.xml",
                "<include><load module='mod_sofia'/><context name='default'><extension name='inbound'><condition><action application='bridge'/></condition></extension></context></include>",
            ),
            (
                "src/mod_sofia.java",
                "public class mod_sofia { public static void bridge() {} }",
            ),
        ],
        false,
    );
    let owner = capability_symbol(
        &facts,
        "freeswitch/dialplan/default.xml",
        "freeswitch::context::default::extension::inbound",
    );
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("bridge", ReferenceKind::Calls);
    assert!(reference.target_symbol_id.is_none());
    assert_eq!(
        reference.resolution_provenance,
        "native-freeswitch-external"
    );
    let module =
        super::capability_reference_in_file(&facts, "freeswitch/dialplan/default.xml", "mod_sofia");
    assert!(module.target_symbol_id.is_none());
    assert_eq!(module.resolution_provenance, "native-freeswitch-external");
}

#[test]
fn freeswitch_includes_resolve_local_files_and_abstain_on_absolute_collisions() {
    let fixtures = [
        (
            "freeswitch/main.xml",
            "<include><X-PRE-PROCESS cmd='include' data='users.xml'/><X-PRE-PROCESS cmd='include' data='/etc/freeswitch/users.xml'/></include>",
        ),
        (
            "freeswitch/users.xml",
            "<include><user id='1000'/></include>",
        ),
        (
            "etc/freeswitch/users.xml",
            "<include><user id='2000'/></include>",
        ),
        (
            "users.xml",
            "<include><domain name='example.invalid'><user id='3000'/></domain></include>",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let local = facts
        .references()
        .iter()
        .find(|reference| reference.reference_name == "users.xml")
        .unwrap_or_else(|| panic!("missing literal relative include"));
    let target = super::capability_file_symbol(&facts, "freeswitch/users.xml");
    assert_eq!(
        local.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{local:?}"
    );
    assert_eq!(
        local.resolution_provenance,
        "native-freeswitch-static-config"
    );
    let external = super::capability_reference_in_file(
        &facts,
        "freeswitch/main.xml",
        "/etc/freeswitch/users.xml",
    );
    assert!(external.target_symbol_id.is_none());
    assert_eq!(
        external.resolution_provenance,
        "native-freeswitch-unresolved"
    );
}

#[test]
fn kamailio_absolute_includes_never_bind_to_project_path_collisions() {
    let fixtures = [
        (
            "kamailio/main.cfg",
            "include_file \"/etc/kamailio/auth.cfg\"\ninclude_file \"C:/kamailio/drive.cfg\"\nrequest_route { route(AUTH); route(DRIVE); }\n",
        ),
        ("etc/kamailio/auth.cfg", "route[AUTH] {}\n"),
        ("kamailio/drive.cfg", "route[DRIVE] {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "kamailio/main.cfg", "kamailio::request_route");
    for name in ["AUTH", "DRIVE"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert!(reference.target_symbol_id.is_none(), "{name}");
    }
    for name in ["/etc/kamailio/auth.cfg", "C:/kamailio/drive.cfg"] {
        let reference = super::capability_reference_in_file(&facts, "kamailio/main.cfg", name);
        assert!(reference.target_symbol_id.is_none(), "{name}");
    }
}

#[test]
fn kamailio_directive_alias_and_route_local_includes_resolve_literal_routes() {
    let fixtures = [
        (
            "kamailio/main.cfg",
            "#!include_file \"auth.cfg\"\nrequest_route {\n include_file \"local.cfg\"\n import_file \"optional.cfg\"\n route(AUTH); route(LOCAL); route(OPTIONAL);\n}\n",
        ),
        ("kamailio/auth.cfg", "route[AUTH] {}\n"),
        ("kamailio/local.cfg", "route[LOCAL] {}\n"),
        ("kamailio/optional.cfg", "route[OPTIONAL] {}\n"),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let owner = capability_symbol(&facts, "kamailio/main.cfg", "kamailio::request_route");
    for (label, file) in [
        ("AUTH", "auth"),
        ("LOCAL", "local"),
        ("OPTIONAL", "optional"),
    ] {
        let target = capability_symbol(
            &facts,
            &format!("kamailio/{file}.cfg"),
            &format!("kamailio::route::{label}"),
        );
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(label, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    }
}
