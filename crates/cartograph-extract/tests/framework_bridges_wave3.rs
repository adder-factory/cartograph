//! Regression coverage for the bounded native bridge forms.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_BYTES: usize = 1024 * 1024;
const MODIFIED_SHARED_FUNCTIONS: &[&str] = &[
    "scan_javascript",
    "scan_native_event_producers",
    "scan_registry_alias_calls",
    "collect_registry_alias_bindings",
    "scan_registry_alias_invocations",
    "legacy_bridge_argument",
    "scan_turbo_module_spec",
    "registry_module_name",
    "scan_codegen_components",
    "record_objc_registration",
    "scan_objc_container",
    "scan_objc_method_macro",
    "scan_native_view_manager",
    "react_native_jvm_module",
    "scan_jvm_view_manager",
    "symbol_has_swift_attribute",
    "swift_objc_selector_attribute",
    "scan_expo_definition",
    "expo_call_argument",
    "objc_module_name",
    "objc_module_registration",
    "react_native_blocklisted",
];

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits =
        SourceLimits::new(SOURCE_BYTES).unwrap_or_else(|error| panic!("source limits: {error}"));
    let snapshot =
        SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("fixture snapshot: {error}"));
    NativeExtractor::new_for_capability_validation(snapshot.language())
        .unwrap_or_else(|error| panic!("fixture extractor: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("fixture extraction: {error}"))
}

fn landmark<'a>(
    file: &'a ExtractedFile,
    identity: &str,
) -> &'a cartograph_extract::ExtractedSymbol {
    let matches: Vec<_> = file
        .symbols
        .iter()
        .filter(|s| s.qualified_name.ends_with(identity))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "{identity}: {:?}",
        file.symbols
            .iter()
            .map(|s| (&s.name, &s.qualified_name))
            .collect::<Vec<_>>()
    );
    matches[0]
}

fn absent(file: &ExtractedFile, identity: &str) {
    assert!(
        file.symbols
            .iter()
            .all(|s| !s.qualified_name.contains(identity)),
        "{identity}: {:?}",
        file.symbols
            .iter()
            .map(|s| (&s.name, &s.qualified_name))
            .collect::<Vec<_>>()
    );
}

#[test]
fn expo_constants_and_emitter_names_belong_to_the_module_subclass() {
    for (path, source) in [
        (
            "ios/Actual.swift",
            "class Helper {}\nclass Actual: Module {\n func definition() -> ModuleDefinition {\n Constants (\"PI\")\n Function (\"addListener\") {}\n MyFunction(\"wrong\") {}\n }\n}",
        ),
        (
            "android/Actual.kt",
            "class Helper {}\nclass Actual: Module() {\n override fun definition() = ModuleDefinition {\n Constants(\"PI\" to 3.14)\n Function (\"addListener\") {}\n MyFunction(\"wrong\") {}\n Constants(\"decoy\" + suffix)\n }\n}",
        ),
    ] {
        let file = extract(path, source);
        assert_eq!(
            landmark(&file, "::expo-module-method::Actual::PI").kind,
            SymbolKind::Method
        );
        landmark(&file, "::expo-module-method::Actual::addListener");
        absent(&file, "::expo-module-method::Actual::wrong");
        absent(&file, "::expo-module-method::Actual::decoy");
        absent(&file, "::expo-module::Helper");
    }
}

#[test]
fn view_properties_require_a_real_manager_and_support_custom_spaced_macros() {
    let file = extract(
        "ios/Foo.m",
        "@implementation Helper\n@end\n@implementation FooManager\nRCT_CUSTOM_VIEW_PROPERTY ( radius, CGFloat, FooView ) {}\nRCT_EXPORT_VIEW_PROPERTY (color, NSString)\n@end\n@implementation LocationManager\n@end",
    );
    landmark(&file, "::native-view-manager::Foo");
    landmark(&file, "::native-view-prop::Foo::radius");
    landmark(&file, "::native-view-prop::Foo::color");
    absent(&file, "::native-view-manager::Location");
    let file = extract(
        "android/Foo.kt",
        "class Helper {}\nclass FooManager: CustomBase() {\n @ReactProp(name = \"color\") fun setColor(value: String) {}\n}\nclass EmptyManager: CustomBase() {}",
    );
    landmark(&file, "::native-view-manager::Foo");
    landmark(&file, "::native-view-prop::Foo::color");
    absent(&file, "::native-view-manager::Empty");
}

#[test]
fn nativeprops_uses_the_structural_interface_or_readonly_object_alias() {
    for declaration in [
        "interface NativeProps { color?: string; click(): void; }",
        "type NativeProps = Readonly<{ color?: string; click(): void; }>",
    ] {
        let source = format!(
            "import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent'; const decoy = 'interface NativeProps {{ wrong: string }}';\nconst metadata = {{ wrong: 1 }};\n{declaration}\nexport default codegenNativeComponent<NativeProps>('Foo');"
        );
        let file = extract("src/FooNativeComponent.ts", &source);
        landmark(&file, "::fabric-prop::color");
        absent(&file, "::fabric-prop::wrong");
        absent(&file, "::fabric-prop::click");
    }
}

#[test]
fn objc_native_exports_allow_spacing_and_class_fallback_without_emitter_exports() {
    let file = extract(
        "ios/RCTActual.m",
        "@interface RCTActual : RCTEventEmitter @end\n@implementation RCTActual\nRCT_EXPORT_METHOD (run:(id)value) {}\nRCT_EXPORT_METHOD(remove) {}\nRCT_EXPORT_METHOD(invalidate) {}\nRCT_EXPORT_METHOD(startObserving) {}\nRCT_EXPORT_METHOD(stopObserving) {}\n@end",
    );
    landmark(&file, "::react-native-module::Actual");
    landmark(&file, "::react-native-method::Actual::run");
    assert_eq!(
        landmark(&file, "::react-native-method::Actual::run")
            .span
            .start_line(),
        3
    );
    for name in ["remove", "invalidate", "startObserving", "stopObserving"] {
        absent(&file, &format!("::react-native-method::Actual::{name}"));
    }
    let file = extract(
        "ios/RCTDecoy.m",
        "@implementation RCTDecoy\n- (void)plain {}\n@end",
    );
    absent(&file, "::react-native-module::");
}

#[test]
fn explicit_module_registration_wins_over_category_class_fallbacks() {
    let file = extract(
        "ios/Actual.m",
        "@implementation RCTActual (Before)\nRCT_EXPORT_METHOD(before) {}\n@end\n@implementation RCTActual\nRCT_EXPORT_MODULE(Chosen)\n@end\n@implementation RCTActual (After)\nRCT_EXPORT_METHOD(after) {}\n@end",
    );
    landmark(&file, "::react-native-method::Chosen::before");
    landmark(&file, "::react-native-method::Chosen::after");
    absent(&file, "::react-native-method::Actual::");
    let file = extract(
        "ios/Actual.m",
        "@implementation RCTActual\nRCT_EXPORT_MODULE(One)\nRCT_EXPORT_METHOD(run) {}\n@end\n@implementation RCTActual (Other)\nRCT_EXPORT_MODULE(Two)\nRCT_EXPORT_METHOD(other) {}\n@end",
    );
    absent(&file, "::react-native-method::");
}

#[test]
fn java_reactmethod_arguments_do_not_hide_the_native_method() {
    let file = extract(
        "android/Actual.java",
        "class Actual { @ReactMethod(isBlockingSynchronousMethod = true) public String syncGet() { return \"value\"; } }",
    );
    landmark(&file, "::react-native-method::Actual::syncGet");
    absent(
        &file,
        "::react-native-method::Actual::isBlockingSynchronousMethod",
    );
}

#[test]
fn turbo_specs_use_top_level_method_nodes_with_or_without_semicolons() {
    let file = extract(
        "src/NativeDevice.ts",
        "const decoy = 'interface Spec { wrong(): void; }';\ninterface OtherSpec { other(): void; }\ninterface Spec extends TurboModule {\n first(): { nested(): void }\n second(): void\n}\nexport default TurboModuleRegistry.getEnforcing<Spec>('Device');",
    );
    landmark(&file, "::turbo-module-spec-method::Device::first");
    landmark(&file, "::turbo-module-spec-method::Device::second");
    for name in ["wrong", "other", "nested"] {
        absent(
            &file,
            &format!("::turbo-module-spec-method::Device::{name}"),
        );
    }
}

#[test]
fn event_subscriptions_support_dotted_handlers_and_spaced_native_sends() {
    let file = extract(
        "src/events.ts",
        "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); import * as handlers from './handlers';\nfunction ready() {}\nemitter.on ('ready', ready);\nclass Listener { subscribe() { emitter.once('done', this.done); } done() {} }\nemitter.addListener ('complete', handlers.complete);\nemitter.on(dynamic, ready);",
    );
    for (event, handler, lookup) in [
        ("ready", "ready", None),
        ("complete", "complete", Some("handlers.complete")),
    ] {
        let owner = landmark(&file, &format!("::react-native-event-consumer::{event}"));
        assert!(
            file.references
                .iter()
                .any(|r| r.owner.as_ref() == Some(&owner.id)
                    && r.kind == ReferenceKind::Calls
                    && r.name == handler
                    && r.resolution_name.as_deref() == lookup)
        );
    }
    let instance = landmark(&file, "::react-native-event-consumer::done");
    assert!(
        file.references
            .iter()
            .all(|reference| reference.owner.as_ref() != Some(&instance.id)
                || reference.kind != ReferenceKind::Calls)
    );
    absent(&file, "::react-native-event-consumer::dynamic");
    let file = extract(
        "ios/Emitter.m",
        "@implementation Emitter\n- (void)publish { [self sendEventWithName : @\"ready\" body:nil]; }\n@end",
    );
    landmark(&file, "::react-native-event-producer::ready");
}

#[test]
fn objc_attributes_inside_a_swift_body_never_expose_plain_methods() {
    let file = extract(
        "ios/Device.swift",
        "class Device {\n func plain() { let text = \"@objc\" }\n @objc func exported() {}\n}\n@objcMembers class Members {\n func inherited() {}\n @nonobjc func hidden() {}\n}",
    );
    absent(&file, "::swift-objc-method::plain::");
    absent(&file, "::swift-objc-method::hidden::");
    landmark(&file, "::swift-objc-method::exported::exported");
    landmark(&file, "::swift-objc-method::inherited::inherited");
}

#[test]
fn registry_factories_use_exact_arguments_and_wrapped_initializer_identity() {
    let file = extract(
        "src/native.ts",
        "const m = (<Spec>requireNativeModule('Feature'));\nm.run();\nTurboModuleRegistry.getEnforcing<(Spec & { tag: 'Wrong' })>('Real');\nTurboModuleRegistry.getDetails<Spec>('Decoy');\nconst text = \"requireNativeModule('Literal')\";",
    );
    landmark(&file, "::native-module-spec::Feature");
    landmark(&file, "::native-module-spec::Real");
    absent(&file, "::native-module-spec::Wrong");
    absent(&file, "::native-module-spec::Decoy");
    absent(&file, "::native-module-spec::Literal");
    assert!(file.references.iter().any(|r| r.name == "run"
        && r.resolution_name.as_deref() == Some("cartograph.native-module-alias::Feature::run")));
}

#[test]
fn repeated_or_shadowed_aliases_abstain_instead_of_crossing_lexical_bindings() {
    for source in [
        "function first() { const m = requireNativeModule('One'); m.run(); }\nfunction second() { const m = requireNativeModule('Two'); m.run(); }",
        "const m = requireNativeModule('One');\n{ function m() {} m.run(); }",
        "const m = requireNativeModule('One');\nfunction f(m) { m.run(); }",
        "const m = requireNativeModule('One');\nm = other; m.run();",
        "function local() { const m = requireNativeModule('One'); m.run(); } function outside() { m.run(); }",
        "const m = requireNativeModule('One'); const C = class m { use() { m.run(); } };",
        "const m = requireNativeModule('One'); import m from './other'; m.run();",
        "const m = requireNativeModule('One'); with (other) { m.run(); }",
        "let m = requireNativeModule('One'); for (m of others) { m.run(); }",
        "let m = requireNativeModule('One'); for (m in others) { m.run(); }",
        "let m = requireNativeModule('One'); for ({value: m} of others) { m.run(); }",
        "const m = requireNativeModule('One'); m.run = replacement; m.run();",
        "const m = requireNativeModule('One'); m['run'] = replacement; m.run();",
        "const m = requireNativeModule('One'); ++m.run; m.run();",
    ] {
        let file = extract("src/native.ts", source);
        assert!(
            file.references.iter().filter(|r| r.name == "run").all(|r| {
                !r.resolution_name.as_deref().is_some_and(|n| {
                    n.contains(cartograph_extract::NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX)
                })
            }),
            "{source}: {:?}",
            file.references
        );
    }
}

#[test]
fn invalid_member_lookup_never_leaves_a_bare_event_handler_reference() {
    let sensitive_receiver = ["gh", "p_", "credential"].concat();
    for expression in [
        "makeHandlers().onReady".to_owned(),
        "handlers . onReady".to_owned(),
        format!("{sensitive_receiver}.onReady"),
    ] {
        let source = format!(
            "import {{NativeEventEmitter}} from 'react-native'; const emitter = new NativeEventEmitter(); import * as handlers from './handlers'; const {sensitive_receiver} = handlers; function onReady() {{}} emitter.on('ready', {expression});"
        );
        let file = extract("src/events.ts", &source);
        let consumer = file
            .symbols
            .iter()
            .find(|symbol| {
                symbol
                    .qualified_name
                    .contains("::react-native-event-consumer::")
            })
            .unwrap_or_else(|| panic!("missing event consumer landmark"));
        assert!(
            file.references
                .iter()
                .all(|reference| reference.owner.as_ref() != Some(&consumer.id)
                    || reference.kind != ReferenceKind::Calls),
            "{expression}: {:?}",
            file.references
        );
    }
}

#[test]
fn native_modules_destructured_and_member_aliases_retain_module_identity() {
    let file = extract(
        "src/native.ts",
        "import {NativeModules} from 'react-native';\nconst {Geolocation, Scanner: Scan} = NativeModules;\nconst Store = NativeModules.Store;\nGeolocation.locate(); Scan.start(); Store.save();",
    );
    for (method, lookup) in [
        ("locate", "Geolocation::locate"),
        ("start", "Scanner::start"),
        ("save", "Store::save"),
    ] {
        let lookup = format!(
            "{}{lookup}",
            cartograph_extract::NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX
        );
        assert!(
            file.references
                .iter()
                .any(|r| r.name == method && r.resolution_name.as_deref() == Some(lookup.as_str())),
            "{method}: {:?}",
            file.references
        );
    }
}

#[test]
fn spec_type_ownership_does_not_borrow_another_registry_or_type_parameter() {
    let file = extract(
        "src/NativeDevice.ts",
        "interface OtherSpec { decoy(): void; }\ninterface Spec extends TurboModule { run(): void; }\nconst other = TurboModuleRegistry.get<OtherSpec>('Wrong');\nexport default TurboModuleRegistry.get<Spec>('Real');",
    );
    landmark(&file, "::turbo-module-spec-method::Real::run");
    absent(&file, "::turbo-module-spec-method::Wrong::run");
    let file = extract(
        "src/NativeDevice.ts",
        "interface Spec extends TurboModule { run(): void; }\nfunction factory<Spec>() { return TurboModuleRegistry.get<Spec>('Wrong'); }",
    );
    absent(&file, "::turbo-module-spec-method::Wrong::run");
}

#[test]
fn native_event_producers_do_not_read_string_or_identifier_decoys() {
    for (path, source) in [
        (
            "ios/Emitter.swift",
            r#"class Emitter { func publish() { let text = "sendEvent(\"wrong\")"; wrongsendEvent("wrong"); sendEvent (withName: "ready", body: nil) } }"#,
        ),
        (
            "android/Emitter.java",
            r#"import DeviceEventManagerModule; class EmitterModule { @ReactMethod public void publish() { String text = "sendEvent(\"wrong\")"; wrongsendEvent("wrong"); sendEvent ("ready", null); } }"#,
        ),
    ] {
        let file = extract(path, source);
        landmark(&file, "::react-native-event-producer::ready");
        absent(&file, "::react-native-event-producer::wrong");
    }
}

#[test]
fn argument_trivia_has_a_fixed_node_bound_and_keeps_small_arguments() {
    const EXCESSIVE_COMMENTS: usize = 100_000;
    let source = format!(
        "requireNativeModule({}'Feature');",
        "/*x*/".repeat(EXCESSIVE_COMMENTS)
    );
    let file = extract("src/oversized.js", &source);
    absent(&file, "::native-module-spec::Feature");
    let file = extract(
        "src/small.js",
        "requireNativeModule(/*x*/ 'Feature' /*x*/);",
    );
    landmark(&file, "::native-module-spec::Feature");
}

#[test]
fn emitter_controls_are_excluded_only_with_typed_emitter_inheritance() {
    for (path, source, module) in [
        (
            "android/File.java",
            "class FileModule { @ReactMethod public void remove(String path) {} @ReactMethod public void addListener() {} @ReactMethod public void removeListeners() {} }",
            "File",
        ),
        (
            "ios/File.m",
            "@implementation RCTFile\nRCT_EXPORT_METHOD(remove:(id)path) {}\nRCT_EXPORT_METHOD(addListener) {}\nRCT_EXPORT_METHOD(removeListeners) {}\n@end",
            "File",
        ),
    ] {
        let file = extract(path, source);
        for name in ["remove", "addListener", "removeListeners"] {
            landmark(&file, &format!("::react-native-method::{module}::{name}"));
        }
    }
    let file = extract(
        "ios/Emitter.m",
        "@interface RCTEmitter : RCTEventEmitter @end\n@implementation RCTEmitter\nRCT_EXPORT_METHOD(remove) {}\nRCT_EXPORT_METHOD(addListener) {}\nRCT_EXPORT_METHOD(removeListeners) {}\nRCT_EXPORT_METHOD(run) {}\n@end",
    );
    landmark(&file, "::react-native-method::Emitter::run");
    for name in ["remove", "addListener", "removeListeners"] {
        absent(&file, &format!("::react-native-method::Emitter::{name}"));
    }
}

#[test]
fn bridge_production_functions_respect_the_track_code_health_limits() {
    let modules = [
        (
            "framework_bridge.rs",
            include_str!("../src/framework_bridge.rs"),
        ),
        (
            "native_bridge_details.rs",
            include_str!("../../cartograph-indexer/src/native_pipeline/native_bridge_details.rs"),
        ),
        (
            "native_event_calls.rs",
            include_str!("../../cartograph-indexer/src/native_pipeline/native_event_calls.rs"),
        ),
        ("forms.rs", include_str!("../src/framework_bridge/forms.rs")),
        (
            "alias_shapes.rs",
            include_str!("../src/framework_bridge/alias_shapes.rs"),
        ),
        (
            "javascript_bindings.rs",
            include_str!("../src/framework_bridge/javascript_bindings.rs"),
        ),
        (
            "native_emitters.rs",
            include_str!("../src/framework_bridge/native_emitters.rs"),
        ),
        (
            "emitter_controls.rs",
            include_str!("../src/framework_bridge/emitter_controls.rs"),
        ),
        (
            "javascript_shapes.rs",
            include_str!("../src/framework_bridge/javascript_shapes.rs"),
        ),
        (
            "events.rs",
            include_str!("../src/framework_bridge/events.rs"),
        ),
        (
            "handler_shapes.rs",
            include_str!("../src/framework_bridge/handler_shapes.rs"),
        ),
        (
            "resolved_handlers.rs",
            include_str!(
                "../../cartograph-indexer/src/native_pipeline/native_event_calls/resolved_handlers.rs"
            ),
        ),
        (
            "require_arguments.rs",
            include_str!("../src/walk/module_system/require_arguments.rs"),
        ),
        (
            "native_alias_refinement.rs",
            include_str!("../src/framework/bridge_transaction/native_alias_refinement.rs"),
        ),
    ];
    let mut issues = Vec::new();
    for (path, source) in modules {
        let extracted = extract(path, source);
        for symbol in extracted
            .symbols
            .iter()
            .filter(|symbol| matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function))
            .filter(|symbol| {
                path != "framework_bridge.rs"
                    || MODIFIED_SHARED_FUNCTIONS.contains(&symbol.name.as_str())
            })
        {
            let lines = symbol.span.end_line() - symbol.span.start_line() + 1;
            let callees = extracted
                .references
                .iter()
                .filter(|r| r.owner.as_ref() == Some(&symbol.id) && r.kind == ReferenceKind::Calls)
                .map(|r| &r.name)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            if symbol.health.cyclomatic >= 15
                || symbol.health.parameter_count > 3
                || lines >= 100
                || callees >= 25
            {
                issues.push(format!(
                    "{path}:{} {} cc={} params={} lines={lines} callees={callees}",
                    symbol.span.start_line(),
                    symbol.qualified_name,
                    symbol.health.cyclomatic,
                    symbol.health.parameter_count
                ));
            }
        }
    }
    assert!(issues.is_empty(), "{}", issues.join("\n"));
}
