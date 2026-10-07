//! Native endpoint, convention, ambiguity and event-channel pipeline regressions.

use super::*;
use std::fmt::Write;

pub(super) fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reverse = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reverse.digest());
    assert_eq!(forward.edges(), reverse.edges());
    assert_eq!(forward.references(), reverse.references());
    forward
}

pub(super) fn edge<'a>(
    facts: &'a CanonicalGenerationFacts,
    query: (&SymbolInput, &SymbolInput, &str),
) -> &'a EdgeInput {
    let (source, target, provenance) = query;
    let matches = facts
        .edges()
        .iter()
        .filter(|e| {
            e.source_symbol_id == source.symbol_id
                && e.target_symbol_id == target.symbol_id
                && e.provenance == provenance
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "{} -> {}: {:?}",
        source.qualified_name,
        target.qualified_name,
        facts.edges()
    );
    matches[0]
}

#[test]
fn jvm_react_calls_target_the_annotated_physical_method_with_convention_provenance() {
    let facts = generation(&[
        (
            "android/StoreModule.java",
            "class StoreModule { @ReactMethod(isBlockingSynchronousMethod = true) public void save() {} public void hidden() {} }",
        ),
        (
            "src/use.ts",
            "function use() { NativeModules.Store.save(); NativeModules.Store.hidden(); }",
        ),
    ]);
    let method = capability_symbol(&facts, "android/StoreModule.java", "StoreModule::save");
    let call = facts
        .references()
        .iter()
        .find(|r| r.reference_name == "save" && r.reference_kind == "calls")
        .unwrap_or_else(|| panic!("missing save Calls reference"));
    assert_eq!(
        call.target_symbol_id.as_ref(),
        Some(&method.symbol_id),
        "{call:?}"
    );
    assert_eq!(
        call.resolution_provenance,
        native_bridge_details::PHYSICAL_METHOD_PROVENANCE
    );
    assert_eq!(
        call.confidence,
        native_bridge_details::CONVENTION_CONFIDENCE
    );
    assert!(
        facts
            .references()
            .iter()
            .filter(|r| r.reference_name.ends_with("hidden"))
            .all(|r| r.target_symbol_id.is_none())
    );
}

#[test]
fn duplicate_native_module_exports_preserve_ambiguity() {
    let facts = generation(&[
        (
            "one/StoreModule.kt",
            "class StoreModule { @ReactMethod fun save() {} }",
        ),
        (
            "two/StoreModule.kt",
            "class StoreModule { @ReactMethod fun save() {} }",
        ),
        ("src/use.ts", "NativeModules.Store.save();"),
    ]);
    assert!(
        facts
            .references()
            .iter()
            .filter(|r| r.reference_name.ends_with("save"))
            .all(|r| r.target_symbol_id.is_none())
    );
}

#[test]
fn apple_aliases_and_native_exports_link_to_their_own_physical_declarations() {
    let facts = generation(&[
        (
            "ios/Worker.m",
            "@implementation Worker\n- (void)other:(id)value {}\n@end",
        ),
        (
            "ios/Player.swift",
            "class Player { @objc func play() {} func hidden() {} }",
        ),
        (
            "ios/Device.m",
            "@implementation Device\nRCT_EXPORT_MODULE(Device)\nRCT_EXPORT_METHOD(run:(id)value) {}\n@end",
        ),
    ]);
    for (path, alias_name, physical_name) in [
        (
            "ios/Worker.m",
            "ios/Worker.m::objc-swift-method::other:::other",
            "Worker::other:",
        ),
        (
            "ios/Player.swift",
            "ios/Player.swift::swift-objc-method::play::play",
            "Player::play",
        ),
        (
            "ios/Device.m",
            "ios/Device.m::react-native-method::Device::run",
            "Device::run:",
        ),
    ] {
        let alias = capability_symbol(&facts, path, alias_name);
        let physical = capability_symbol(&facts, path, physical_name);
        let link = edge(
            &facts,
            (
                alias,
                physical,
                native_bridge_details::PHYSICAL_METHOD_PROVENANCE,
            ),
        );
        assert_eq!(link.kind, EdgeKind::References);
        assert_eq!(
            link.confidence,
            native_bridge_details::CONVENTION_CONFIDENCE
        );
    }
    let hidden = capability_symbol(&facts, "ios/Player.swift", "Player::hidden");
    assert!(
        facts
            .edges()
            .iter()
            .all(|e| e.target_symbol_id != hidden.symbol_id
                || e.provenance != native_bridge_details::PHYSICAL_METHOD_PROVENANCE)
    );
}

const REMOVED_FABRIC_PROVENANCE: &str = "framework-fabric-native-implementation";

#[test]
fn fabric_basename_matches_without_ownership_never_target_native_classes() {
    let facts = generation(&[
        (
            "src/FooNativeComponent.ts",
            "import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent'; export default codegenNativeComponent('Foo');",
        ),
        (
            "native/utilities.hpp",
            "namespace utilities { class Foo {}; }",
        ),
        ("android/FooView.kt", "class FooView {}"),
        (
            "ios/FooComponentView.mm",
            "@implementation FooComponentView\n@end",
        ),
    ]);
    let component = capability_symbol(
        &facts,
        "src/FooNativeComponent.ts",
        "src/FooNativeComponent.ts::fabric-component::Foo",
    );
    let utility = capability_symbol(&facts, "native/utilities.hpp", "utilities::Foo");
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.provenance != REMOVED_FABRIC_PROVENANCE)
    );
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.source_symbol_id != component.symbol_id
                || edge.target_symbol_id != utility.symbol_id)
    );
}

#[test]
fn fabric_duplicate_basenames_also_abstain() {
    let facts = generation(&[
        (
            "src/Foo.ts",
            "import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent'; codegenNativeComponent('Foo');",
        ),
        ("one/FooView.kt", "class FooView {}"),
        ("two/FooView.kt", "class FooView {}"),
    ]);
    capability_symbol(&facts, "src/Foo.ts", "src/Foo.ts::fabric-component::Foo");
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.provenance != REMOVED_FABRIC_PROVENANCE)
    );
}

#[test]
fn native_dispatchers_call_the_exact_resolved_event_handler() {
    let facts = generation(&[
        (
            "ios/Emitter.m",
            "@implementation Emitter\n- (void)publish { [self sendEventWithName : @\"ready\" body:nil]; }\n- (void)unmatched { [self sendEventWithName:@\"missing\" body:nil]; }\n@end",
        ),
        (
            "src/events.ts",
            "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter();\nfunction onReady() {}\nfunction other() {}\nemitter.on('ready', onReady); emitter.once ('ready', onReady); emitter.on(dynamic, other);",
        ),
    ]);
    let source = capability_symbol(&facts, "ios/Emitter.m", "Emitter::publish");
    let target = capability_symbol(&facts, "src/events.ts", "onReady");
    let link = edge(&facts, (source, target, native_event_calls::PROVENANCE));
    assert_eq!(link.kind, EdgeKind::Calls);
    assert_eq!(
        link.confidence,
        native_bridge_details::CONVENTION_CONFIDENCE
    );
    let unmatched = capability_symbol(&facts, "ios/Emitter.m", "Emitter::unmatched");
    assert!(
        facts
            .edges()
            .iter()
            .all(|e| e.source_symbol_id != unmatched.symbol_id
                || e.provenance != native_event_calls::PROVENANCE)
    );
}

#[test]
fn native_event_fanout_over_six_endpoints_abstains() {
    let mut native = String::new();
    for n in 0..=native_event_calls::MAX_ENDPOINTS {
        writeln!(
            native,
            "- (void)publish{n} {{ [self sendEventWithName:@\"ready\" body:nil]; }}"
        )
        .unwrap_or_else(|error| panic!("event producer source: {error}"));
    }
    let native = format!("@implementation Emitter\n{native}@end");
    let facts = generation(&[
        ("ios/Emitter.m", &native),
        (
            "src/events.ts",
            "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter();\nfunction onReady() {}\nemitter.on('ready', onReady);",
        ),
    ]);
    assert!(
        facts
            .edges()
            .iter()
            .all(|e| e.provenance != native_event_calls::PROVENANCE
                && e.provenance != NATIVE_EVENT_BRIDGE_PROVENANCE)
    );
}

#[test]
fn native_event_fanout_over_six_distinct_handlers_abstains() {
    let mut source = "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter();\n".to_owned();
    for n in 0..=native_event_calls::MAX_ENDPOINTS {
        writeln!(
            source,
            "function handler{n}() {{}} emitter.on('ready', handler{n});"
        )
        .unwrap_or_else(|error| panic!("event handler source: {error}"));
    }
    let facts = generation(&[
        ("src/events.ts", &source),
        (
            "ios/Emitter.m",
            "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"ready\" body:nil]; }\n@end",
        ),
    ]);
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.provenance != native_event_calls::PROVENANCE
                && edge.provenance != NATIVE_EVENT_BRIDGE_PROVENANCE)
    );
}

#[test]
fn commonjs_comment_trivia_preserves_the_exact_module_target() {
    let facts = generation(&[
        ("src/m.js", "function save() {} module.exports = {save};"),
        (
            "src/decoy.js",
            "function save() {} module.exports = {save};",
        ),
        (
            "src/use.js",
            "const m = require(/* comment */ './m' /* trailing */); function run() { m.save(); }",
        ),
        (
            "src/extra.js",
            "const m = require('./m', './decoy'); function run() { m.save(); }",
        ),
        (
            "src/shadow.js",
            "function require(value) { return value; } const m = require(/* comment */ './m'); function run() { m.save(); }",
        ),
    ]);
    let target = capability_symbol(&facts, "src/m.js", "save");
    let reference = capability_reference_in_file(&facts, "src/use.js", "m.save");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    for path in ["src/extra.js", "src/shadow.js"] {
        assert!(
            capability_reference_in_file(&facts, path, "m.save")
                .target_symbol_id
                .is_none()
        );
    }
}

#[test]
fn wrapped_registry_aliases_and_native_module_aliases_target_their_own_exports() {
    let facts = generation(&[
        (
            "android/Feature.kt",
            "class FeatureModule { @ReactMethod fun run() {} }",
        ),
        (
            "android/Other.kt",
            "class OtherModule { @ReactMethod fun run() {} }",
        ),
        (
            "src/use.ts",
            "const m = (<Spec>requireNativeModule('Feature')); const {Other: other} = NativeModules; function use() { m.run(); other.run(); }",
        ),
        (
            "src/shadow.ts",
            "const m = requireNativeModule('Feature'); function use(m: unknown) { m.run(); }",
        ),
    ]);
    let feature = capability_symbol(&facts, "android/Feature.kt", "FeatureModule::run");
    let other = capability_symbol(&facts, "android/Other.kt", "OtherModule::run");
    let file = facts
        .files()
        .iter()
        .find(|f| f.normalized_path == "src/use.ts")
        .unwrap_or_else(|| panic!("missing src/use.ts fixture file"));
    let calls = facts
        .references()
        .iter()
        .filter(|r| r.file_id == file.file_id && r.reference_name == "run")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].target_symbol_id.as_ref(),
        Some(&feature.symbol_id),
        "{calls:?}"
    );
    assert_eq!(calls[1].target_symbol_id.as_ref(), Some(&other.symbol_id));
    assert!(
        calls
            .iter()
            .all(|r| r.resolution_provenance == native_bridge_details::PHYSICAL_METHOD_PROVENANCE)
    );
    assert!(
        capability_reference_in_file(&facts, "src/shadow.ts", "m.run")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn semicolonless_spec_methods_link_to_physical_implementations() {
    let facts = generation(&[
        (
            "src/NativeFeature.ts",
            "interface OtherSpec { wrong(): void } interface Spec extends TurboModule { run(): void\n stop(): void } const other = TurboModuleRegistry.get<OtherSpec>('Wrong'); export default TurboModuleRegistry.get<Spec>('Feature');",
        ),
        (
            "android/Feature.kt",
            "class FeatureModule {\n @ReactMethod fun run() {}\n @ReactMethod fun stop() {}\n fun hidden() {}\n}",
        ),
    ]);
    for method in ["run", "stop"] {
        let spec_name =
            format!("src/NativeFeature.ts::turbo-module-spec-method::Feature::{method}");
        let native_name = format!("FeatureModule::{method}");
        let link = edge(
            &facts,
            (
                capability_symbol(&facts, "src/NativeFeature.ts", &spec_name),
                capability_symbol(&facts, "android/Feature.kt", &native_name),
                TURBO_NATIVE_BRIDGE_PROVENANCE,
            ),
        );
        assert_eq!(link.confidence, FRAMEWORK_CONVENTION_CONFIDENCE);
        assert_eq!(link.kind, EdgeKind::References);
    }
    assert!(facts.symbols().iter().all(|s| {
        !s.qualified_name
            .contains("::turbo-module-spec-method::Wrong::")
    }));
}

#[test]
fn producer_text_and_noncontained_dispatches_never_call_a_handler() {
    let facts = generation(&[
        (
            "ios/Emitter.m",
            r#"@implementation Emitter
- (void)fake { id text = @"sendEventWithName:@\"ready\""; }
@end
void outside() {}
[self sendEventWithName:@"ready" body:nil];"#,
        ),
        (
            "src/events.ts",
            "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); function onReady() {} emitter.on('ready', onReady);",
        ),
    ]);
    assert!(
        facts
            .edges()
            .iter()
            .all(|e| e.provenance != native_event_calls::PROVENANCE)
    );
}

#[test]
fn missing_or_ambiguous_alias_hints_keep_existing_dynamic_resolution() {
    for module in ["Missing", "Feature"] {
        let source = format!(
            "const m = requireNativeModule('{module}'); function use() {{ m.run(); }} function run() {{}}"
        );
        let facts = generation(&[
            ("src/use.ts", &source),
            (
                "one/Feature.kt",
                "class FeatureModule { @ReactMethod fun run() {} }",
            ),
            (
                "two/Feature.kt",
                "class FeatureModule { @ReactMethod fun run() {} }",
            ),
        ]);
        let target = capability_symbol(&facts, "src/use.ts", "run");
        let call = facts
            .references()
            .iter()
            .find(|r| r.reference_name == "run" && r.reference_kind == "calls")
            .unwrap_or_else(|| panic!("missing run Calls reference"));
        assert_eq!(
            call.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{call:?}"
        );
        assert_eq!(call.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
        assert_eq!(call.confidence, DYNAMIC_DISPATCH_CONFIDENCE);
    }
}

#[test]
fn dotted_event_handlers_require_their_actual_receiver_target() {
    let facts = generation(&[
        (
            "ios/Emitter.m",
            "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"ready\" body:nil]; }\n- (void)wrong { [self sendEventWithName:@\"wrong\" body:nil]; }\n@end",
        ),
        ("src/handlers.ts", "export function onReady() {}"),
        (
            "src/events.ts",
            "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); import * as handlers from './handlers'; function wrongHandler() {} emitter.on('ready', handlers.onReady); emitter.on('wrong', foreign.wrongHandler); class Listener { subscribe() { emitter.once('ready', this.onReady); } onReady() {} }",
        ),
    ]);
    let source = capability_symbol(&facts, "ios/Emitter.m", "Emitter::publish");
    let target = capability_symbol(&facts, "src/handlers.ts", "onReady");
    edge(&facts, (source, target, native_event_calls::PROVENANCE));
    let instance = capability_symbol(&facts, "src/events.ts", "Listener::onReady");
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.target_symbol_id != instance.symbol_id
                || edge.provenance != native_event_calls::PROVENANCE)
    );
    let wrong = capability_symbol(&facts, "ios/Emitter.m", "Emitter::wrong");
    assert!(
        facts
            .edges()
            .iter()
            .all(|e| e.source_symbol_id != wrong.symbol_id
                || e.provenance != native_event_calls::PROVENANCE)
    );
}

#[test]
fn unproven_this_handlers_never_fall_back_to_a_same_named_function() {
    for subscription in [
        "emitter.on('wrong', this.wrongHandler); function wrongHandler() {}",
        "class Listener { subscribe() { function nested() { emitter.once('wrong', this.wrongHandler); } } wrongHandler() {} } function wrongHandler() {}",
        "function outer() { class Listener { subscribe() { emitter.addListener('wrong', this.wrongHandler); } wrongHandler() {} } } function wrongHandler() {}",
        "class Listener { subscribe() { function* nested() { emitter.once('wrong', this.wrongHandler); } } wrongHandler() {} }",
        "class Listener { subscribe() { const object = { nested() { emitter.once('wrong', this.wrongHandler); } }; } wrongHandler() {} }",
        "class Listener { subscribe() { const C = class { nested() { emitter.once('wrong', this.wrongHandler); } }; } wrongHandler() {} }",
        "class Listener { static subscribe() { emitter.once('wrong', this.wrongHandler); } wrongHandler() {} }",
        "class Listener { subscribe(this: Other) { emitter.once('wrong', this.wrongHandler); } wrongHandler() {} }",
        "class Listener { static { emitter.once('wrong', this.wrongHandler); } wrongHandler() {} }",
    ] {
        let source = format!(
            "import {{NativeEventEmitter}} from 'react-native'; const emitter = new NativeEventEmitter(); {subscription}"
        );
        let facts = generation(&[
            (
                "ios/Emitter.m",
                "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"wrong\" body:nil]; }\n@end",
            ),
            ("src/events.ts", &source),
        ]);
        assert!(
            facts
                .edges()
                .iter()
                .all(|e| e.provenance != native_event_calls::PROVENANCE),
            "{subscription}: {:?}",
            facts.edges()
        );
    }
}

#[test]
fn shadowed_event_callbacks_never_bind_unrelated_project_functions() {
    let facts = generation(&[
        (
            "ios/Emitter.m",
            "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"wrong\" body:nil]; }\n@end",
        ),
        ("src/handler.ts", "export function onReady() {}"),
        (
            "src/events.ts",
            "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); function subscribe(onReady: unknown) { emitter.on('wrong', onReady); }",
        ),
    ]);
    assert!(
        facts
            .edges()
            .iter()
            .all(|e| e.provenance != native_event_calls::PROVENANCE),
        "{:?}",
        facts.edges()
    );
}

#[test]
fn unsupported_or_shadowed_member_handlers_never_call_an_unrelated_leaf() {
    for subscription in [
        "function onReady() {} emitter.on('wrong', makeHandlers().onReady);",
        "import * as handlers from './handler'; function onReady() {} emitter.on('wrong', handlers . onReady);",
        "import * as handlers from './handler'; function subscribe(handlers: unknown) { emitter.on('wrong', handlers.onReady); }",
        "function onReady() {} function subscribe(onReady: unknown) { emitter.on('wrong', onReady); }",
        "class Listener { subscribe() { emitter.on('wrong', this.onReady); } } function onReady() {}",
        "const handlers = unknown; function onReady() {} emitter.on('wrong', handlers.onReady);",
    ] {
        let source = format!(
            "import {{NativeEventEmitter}} from 'react-native'; const emitter = new NativeEventEmitter(); {subscription}"
        );
        let facts = generation(&[
            ("src/handler.ts", "export function onReady() {}"),
            ("src/events.ts", &source),
            (
                "ios/Emitter.m",
                "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"wrong\" body:nil]; }\n@end",
            ),
        ]);
        assert!(
            facts
                .edges()
                .iter()
                .all(|e| e.provenance != native_event_calls::PROVENANCE),
            "{subscription}: {:?}",
            facts.edges()
        );
    }
}

#[test]
fn loop_rebindings_and_member_overwrites_withdraw_native_alias_resolution() {
    for statement in [
        "for (m of others) { m.run(); }",
        "for (m in others) { m.run(); }",
        "for ({value: m} of others) { m.run(); }",
        "m.run = replacement; m.run();",
        "m['run'] = replacement; m.run();",
        "++m.run; m.run();",
        "delete m.run; m.run();",
        "[m.run] = [replacement]; m.run();",
        "({x: m.run} = {x: replacement}); m.run();",
        "(eval)('m.run = replacement'); m.run();",
    ] {
        let source = format!("let m = requireNativeModule('Feature'); {statement}");
        let facts = generation(&[
            ("src/use.ts", &source),
            (
                "android/Feature.kt",
                "class FeatureModule { @ReactMethod fun run() {} }",
            ),
        ]);
        let native = capability_symbol(&facts, "android/Feature.kt", "FeatureModule::run");
        assert!(
            facts
                .references()
                .iter()
                .all(|reference| reference.resolution_provenance
                    != native_bridge_details::ALIAS_PROVENANCE
                    && reference.target_symbol_id.as_ref() != Some(&native.symbol_id)),
            "{statement}: {:?}",
            facts.references()
        );
    }
}
