//! Telephony configuration structure stays searchable without retaining option values.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const LIMIT: usize = 1024 * 1024;
const SECRET: &str = "telephony_literal_secret_sentinel_719";

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("extractor: {error}"));
    let file = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extract: {error}"));
    assert_eq!(
        file,
        extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("repeat: {error}"))
    );
    assert!(!format!("{file:?}").contains(SECRET));
    file
}

#[test]
fn freeswitch_xml_has_scoped_dialplan_and_profile_facts() {
    let file = extract(
        "conf/freeswitch/dialplan/default.xml",
        r#"<include>
<context name="default"><extension name="inbound">
  <condition field="destination_number" expression="^1000$">
    <action application="answer"/>
    <action application="bridge" data="telephony_literal_secret_sentinel_719"/>
  </condition>
</extension></context>
<profile name="internal"><settings><param name="password" value="telephony_literal_secret_sentinel_719"/></settings></profile>
<X-PRE-PROCESS cmd="include" data="../directory/users.xml"/>
</include>"#,
    );
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    let context = file
        .symbols
        .iter()
        .find(|s| s.name == "default")
        .unwrap_or_else(|| panic!("context"));
    let extension = file
        .symbols
        .iter()
        .find(|s| s.name == "inbound")
        .unwrap_or_else(|| panic!("extension"));
    assert_eq!(extension.kind, SymbolKind::Function);
    assert_eq!(
        extension.qualified_name,
        "freeswitch::context::default::extension::inbound"
    );
    assert_eq!(extension.span.start_line(), 2);
    assert_eq!(extension.span.end_line(), 7);
    assert!(
        file.containments
            .iter()
            .any(|c| c.parent == context.id && c.child == extension.id)
    );
    assert!(
        file.symbols
            .iter()
            .any(|s| s.name == "password" && s.kind == SymbolKind::Property)
    );
    assert!(
        file.references
            .iter()
            .any(|r| r.owner.as_ref() == Some(&extension.id)
                && r.name == "bridge"
                && r.kind == ReferenceKind::Calls)
    );
    assert!(
        file.import_bindings
            .iter()
            .any(|b| b.module_specifier == "../directory/users.xml")
    );
}

#[test]
fn kamailio_routes_calls_modules_and_defines_are_literal_safe() {
    let file = extract(
        "etc/kamailio/kamailio.cfg",
        r#"#!KAMAILIO
#!define WITH_AUTH
include_file "routes.cfg"
loadmodule "tm.so"
modparam("auth_db", "db_url", "telephony_literal_secret_sentinel_719")
request_route {
    route(AUTH);
    t_on_failure("RETRY");
    xlog("telephony_literal_secret_sentinel_719 fake_call()");
    // route(COMMENT);
}
route[AUTH] { if (is_method("INVITE")) { t_relay(); } }
failure_route[RETRY] { route(AUTH); }
event_route[core:worker-one-init] { return; }
"#,
    );
    assert_eq!(file.language.as_str(), "kamailio");
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    for name in [
        "WITH_AUTH",
        "request_route",
        "AUTH",
        "RETRY",
        "core:worker-one-init",
        "db_url",
    ] {
        assert!(file.symbols.iter().any(|s| s.name == name), "{name}");
    }
    assert!(file.references.iter().any(|r| r.name == "AUTH"
        && r.kind == ReferenceKind::Calls
        && r.resolution_name.as_deref() == Some("kamailio::route::AUTH")));
    assert!(
        file.references
            .iter()
            .any(|r| r.resolution_name.as_deref() == Some("kamailio::failure_route::RETRY"))
    );
    assert!(file.references.iter().any(|r| r.name == "t_relay"));
    assert!(
        !file
            .references
            .iter()
            .any(|r| matches!(r.name.as_str(), "fake_call" | "COMMENT"))
    );
    assert!(
        file.import_bindings
            .iter()
            .any(|b| b.module_specifier == "routes.cfg")
    );
}

#[test]
fn rtpengine_sections_own_keys_and_preserve_repeated_options() {
    let file = extract(
        "etc/rtpengine/rtpengine.conf",
        "[rtpengine]\ninterface=internal/10.0.0.1;external/203.0.113.1\nlisten-ng=127.0.0.1:2223\nredis=telephony_literal_secret_sentinel_719\n[templates]\nWebRTC=ICE=force RTP/SAVPF\n[alternate]\ninterface=127.0.0.1\n",
    );
    assert_eq!(file.language.as_str(), "rtpengine");
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    for qualified in [
        "rtpengine::rtpengine::interface",
        "rtpengine::rtpengine::listen-ng",
        "rtpengine::rtpengine::redis",
        "rtpengine::templates::WebRTC",
        "rtpengine::alternate::interface",
    ] {
        assert!(
            file.symbols.iter().any(|s| s.qualified_name == qualified),
            "{qualified}"
        );
    }
    assert_eq!(
        file.symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Namespace)
            .count(),
        3
    );
    assert_eq!(file.containments.len(), 5);
}

#[test]
fn ordinary_xml_remains_outside_the_freeswitch_scanner() {
    let file = extract(
        "src/settings.xml",
        "<configuration><context name=\"not-telephony\"/><param name=\"arbitrary\" value=\"ignored\"/></configuration>",
    );
    assert_eq!(file.symbols, []);
}

#[test]
fn standalone_freeswitch_documents_and_fragments_are_recognized_by_structure() {
    for source in [
        "<?xml version=\"1.0\"?><document type='freeswitch/xml'><section name='directory'><domain name='example.invalid'><groups><group name='default'><users><user id='1000'><variables><variable name='user_context' value='telephony_literal_secret_sentinel_719'/></variables></user></users></group></groups></domain></section></document>",
        "<configuration name='sofia.conf'><profiles><profile name='external'/></profiles></configuration>",
        "<include><profile name='external'><settings><param name='context' value='public'/></settings></profile></include>",
        "<include><gateway name='carrier'><param name='password' value='telephony_literal_secret_sentinel_719'/></gateway></include>",
        "<include><context name='public'><extension name='inbound'/></context></include>",
        "<include><domain name='example.invalid'><user id='1000'/></domain></include>",
    ] {
        let file = extract("generated/document.xml", source);
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
        assert_ne!(file.symbols, []);
    }
    let file = extract(
        "generated/preprocessor.xml",
        "<include><X-PRE-PROCESS cmd='set' data='default_password=telephony_literal_secret_sentinel_719'/><X-PRE-PROCESS cmd='env-set' data='sip_port=$PORT'/><X-PRE-PROCESS cmd='include' data='${dynamic_path}'/><load module='mod_sofia'/></include>",
    );
    assert!(
        file.symbols
            .iter()
            .any(|symbol| symbol.qualified_name == "freeswitch::variable::default_password")
    );
    assert!(
        file.symbols
            .iter()
            .any(|symbol| symbol.qualified_name == "freeswitch::variable::sip_port")
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "mod_sofia"
                && reference.kind == ReferenceKind::Imports)
    );
    assert_eq!(file.import_bindings, []);
}

#[test]
fn kamailio_multiline_headers_callbacks_and_configuration_keys_are_scoped() {
    let file = extract(
        "edge.cfg",
        r#"#!KAMAILIO
#!trydef WITH_NAT
#!redefine WITH_TLS
#!include "local.cfg"
debug=2
request_route {
    route("AUTH");
    t_on_branch('BRANCH');
    t_on_reply("REPLY");
    route($var(dynamic));
    xlog("é \\\" escaped quote");
}
route
 [ "AUTH" ]
 { return; }
branch_route[BRANCH] {}
onreply_route[REPLY] {}
"#,
    );
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    for expected in [
        "kamailio::define::WITH_NAT",
        "kamailio::define::WITH_TLS",
        "kamailio::core::debug",
        "kamailio::route::AUTH",
    ] {
        assert!(
            file.symbols
                .iter()
                .any(|symbol| symbol.qualified_name == expected),
            "{expected}"
        );
    }
    for expected in [
        "kamailio::branch_route::BRANCH",
        "kamailio::onreply_route::REPLY",
        "kamailio::builtin::route",
    ] {
        assert!(
            file.references
                .iter()
                .any(|reference| reference.resolution_name.as_deref() == Some(expected)),
            "{expected}"
        );
    }
    assert!(
        file.import_bindings
            .iter()
            .any(|binding| binding.module_specifier == "local.cfg")
    );
}

#[test]
fn kamailio_continued_macros_are_opaque_and_include_aliases_are_retained() {
    let source = r#"#!KAMAILIO
#!define BODY \
 route[FAKE] { \
 fake_call(); include_file "fake.cfg"; }
#!include_file "real.cfg"
request_route {
 include_file "local.cfg"
 import_file "optional.cfg"
 route(REAL);
}
route[REAL] {}
"#;
    for source in [
        source.to_owned(),
        source.replace('\n', "\r\n"),
        source.replace("#!", "!!"),
    ] {
        let file = extract("kamailio/macros.cfg", &source);
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
        assert!(file.symbols.iter().any(|symbol| symbol.name == "BODY"));
        assert!(file.symbols.iter().any(|symbol| symbol.name == "REAL"));
        assert!(!file.symbols.iter().any(|symbol| symbol.name == "FAKE"));
        assert!(
            !file
                .references
                .iter()
                .any(|reference| reference.name == "fake_call")
        );
        let includes: Vec<_> = file
            .import_bindings
            .iter()
            .map(|binding| binding.module_specifier.as_str())
            .collect();
        assert_eq!(includes, ["real.cfg", "local.cfg", "optional.cfg"]);
    }
}

#[test]
fn scanner_capacity_exhaustion_is_explicit() {
    use cartograph_extract::ExtractError;
    for (path, source) in [
        ("kamailio/dense.cfg", ";".repeat(200_001)),
        ("freeswitch/dense.xml", "<i/>".repeat(100_001)),
    ] {
        let limits = SourceLimits::new(LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("extractor: {error}"));
        assert_eq!(extractor.extract(&snapshot), Err(ExtractError::OutputLimit));
    }
}

#[test]
fn malformed_configuration_is_partial_and_opaque_regions_emit_no_facts() {
    for (path, source) in [
        ("kamailio/broken.cfg", "request_route { route(AUTH);"),
        ("kamailio/comment.cfg", "/* unterminated"),
        ("kamailio/string.cfg", "request_route { xlog(\"unterminated"),
        (
            "freeswitch/broken.xml",
            "<include><context name=\"default\"></include>",
        ),
        ("freeswitch/unclosed.xml", "<include><!-- unclosed"),
        ("rtpengine/broken.conf", "[rtpengine\ninterface=lo\n"),
        ("rtpengine/orphan.conf", "interface=lo\n"),
    ] {
        assert_eq!(
            extract(path, source).parse_status,
            FileParseStatus::Partial,
            "{path}"
        );
    }
    let xml = extract(
        "freeswitch/opaque.xml",
        "<include><!-- <context name=\"comment\"/> --><![CDATA[<context name=\"cdata\"/>]]><context name=\"real\"/></include>",
    );
    assert_eq!(
        xml.symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["real"]
    );
    let cfg = extract(
        "kamailio/opaque.cfg",
        "#!define MACRO route[FAKE] { fake_call(); }\n/* route[COMMENT] {} */\nrequest_route { $var(flag) = 1; xlog(\"route(STRING)\"); }\n",
    );
    assert!(
        !cfg.symbols
            .iter()
            .any(|symbol| matches!(symbol.name.as_str(), "FAKE" | "COMMENT"))
    );
    assert!(
        !cfg.references
            .iter()
            .any(|reference| matches!(reference.name.as_str(), "fake_call" | "var" | "STRING"))
    );
}

#[test]
fn telephony_scanners_cancel_inside_large_opaque_regions_and_enforce_depth() {
    use cartograph_extract::ExtractError;
    for (path, source) in [
        (
            "kamailio/comment.cfg",
            format!("/*{}*/", "x".repeat(65_536)),
        ),
        (
            "kamailio/directive.cfg",
            format!("#!{}", "x".repeat(65_536)),
        ),
        (
            "freeswitch/comment.xml",
            format!("<!--{}-->", "x".repeat(65_536)),
        ),
        (
            "kamailio/quoted.cfg",
            format!("request_route {{ xlog(\"{}\"); }}", "x".repeat(65_536)),
        ),
        (
            "rtpengine/many.conf",
            format!("[rtpengine]\n{}", "interface=lo\n".repeat(5_000)),
        ),
    ] {
        let limits = SourceLimits::new(LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("extractor: {error}"));
        let mut polls = 0;
        assert_eq!(
            extractor.extract_with_cancellation(&snapshot, || {
                polls += 1;
                polls > 8
            }),
            Err(ExtractError::Cancelled),
            "{path}"
        );
    }
    for (path, source) in [
        (
            "freeswitch/deep.xml",
            format!("{}{}", "<include>".repeat(65), "</include>".repeat(65)),
        ),
        (
            "kamailio/deep.cfg",
            format!(
                "request_route {{ {} {} }}",
                "if (1) {".repeat(65),
                "}".repeat(65)
            ),
        ),
    ] {
        let limits = SourceLimits::new(LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .and_then(|extractor| extractor.with_maximum_ast_depth(64))
            .unwrap_or_else(|error| panic!("extractor: {error}"));
        let file = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("depth: {error}"));
        assert_ne!(file.parse_status, FileParseStatus::Parsed);
        assert!(file.diagnostics.iter().any(|diagnostic| diagnostic.code
            == cartograph_extract::DiagnosticCode::NestingLimitExceeded));
    }
}
