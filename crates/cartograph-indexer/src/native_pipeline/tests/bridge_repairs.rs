//! Review counterexamples exercised through extraction and canonical resolution.

use super::bridge_details::{edge, generation};
use super::*;

const EVENTS_PATH: &str = "src/events.ts";
const PRODUCER_PATH: &str = "ios/Emitter.m";
const PRODUCER: &str = "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"ready\" body:nil]; }\n@end";
const NATIVE_EMITTER: &str =
    "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter();";
const CODEGEN_IMPORT: &str =
    "import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent';";

fn event_generation(source: &str) -> CanonicalGenerationFacts {
    generation(&[(EVENTS_PATH, source), (PRODUCER_PATH, PRODUCER)])
}

fn no_native_callback(facts: &CanonicalGenerationFacts) {
    assert!(
        facts
            .edges()
            .iter()
            .all(|edge| edge.provenance != native_event_calls::PROVENANCE),
        "{:?}",
        facts.edges()
    );
}

fn call<'facts>(
    facts: &'facts CanonicalGenerationFacts,
    (path, name): (&str, &str),
) -> &'facts ReferenceInput {
    let file = facts
        .files()
        .iter()
        .find(|file| file.normalized_path == path)
        .unwrap_or_else(|| panic!("missing file {path}"));
    facts
        .references()
        .iter()
        .find(|reference| {
            reference.file_id == file.file_id
                && reference.reference_kind == "calls"
                && reference.reference_name == name
        })
        .unwrap_or_else(|| panic!("missing Calls {path}::{name}: {:?}", facts.references()))
}

#[test]
fn wrapped_alias_writes_withdraw_the_expo_alias_hint() {
    for receiver in [
        "(m)",
        "(m as Spec)",
        "(m satisfies Spec)",
        "m!",
        "(other, m)",
    ] {
        let source = format!(
            "import {{requireNativeModule}} from 'expo-modules-core'; const m = (requireNativeModule('Feature')); function replacement() {{}} {receiver}.run = replacement; m.run();"
        );
        let facts = generation(&[
            ("src/use.ts", &source),
            (
                "android/Feature.kt",
                "class Feature : Module() { override fun definition() = ModuleDefinition { Name(\"Feature\"); Function(\"run\") {} } }",
            ),
        ]);
        capability_symbol(
            &facts,
            "android/Feature.kt",
            "android/Feature.kt::expo-module-method::Feature::run",
        );
        assert!(
            facts
                .references()
                .iter()
                .all(|reference| reference.resolution_provenance
                    != native_bridge_details::ALIAS_PROVENANCE),
            "{receiver}: {:?}",
            facts.references()
        );
    }
    let facts = generation(&[
        (
            "src/use.js",
            "import {requireNativeModule} from 'expo-modules-core'; const m = (requireNativeModule('Feature')); m.run();",
        ),
        (
            "android/Feature.kt",
            "class Feature : Module() { override fun definition() = ModuleDefinition { Name(\"Feature\"); Function(\"run\") {} } }",
        ),
    ]);
    let native = capability_symbol(
        &facts,
        "android/Feature.kt",
        "android/Feature.kt::expo-module-method::Feature::run",
    );
    let call = call(&facts, ("src/use.js", "run"));
    assert_eq!(call.target_symbol_id.as_ref(), Some(&native.symbol_id));
    assert_eq!(
        call.resolution_provenance,
        native_bridge_details::ALIAS_PROVENANCE
    );
}

#[test]
fn instance_and_prototype_writes_never_qualify_this_callbacks() {
    for declaration in [
        "function replacement() {} class Listener { onReady() {} constructor() { this.onReady = replacement; } subscribe() { emitter.on('ready', this.onReady); } }",
        "function replacement() {} class Listener { onReady() {} subscribe() { emitter.on('ready', this.onReady); } } Listener.prototype.onReady = replacement;",
        "class Listener { onReady() {} subscribe() { emitter.on('ready', this.onReady); } }",
    ] {
        let source = format!("{NATIVE_EMITTER} {declaration}");
        let facts = event_generation(&source);
        capability_symbol(
            &facts,
            EVENTS_PATH,
            "src/events.ts::react-native-event-consumer::ready",
        );
        capability_symbol(&facts, EVENTS_PATH, "Listener::onReady");
        no_native_callback(&facts);
    }
}

#[test]
fn only_proven_native_emitter_receivers_join_the_native_channel() {
    for declaration in [
        "import {NativeEventEmitter} from 'react-native'; import {EventEmitter} from 'events'; const bus = new EventEmitter(); function onReady() {} bus.on('ready', onReady);",
        "import {NativeEventEmitter} from 'react-native'; function onReady() {} unknown.on('ready', onReady);",
        "import {NativeEventEmitter} from 'events'; const bus = new NativeEventEmitter(); function onReady() {} bus.on('ready', onReady);",
        "import {NativeEventEmitter} from 'react-native'; function subscribe(bus) { bus.on('ready', onReady); } function onReady() {}",
        "import {NativeEventEmitter} from 'react-native'; { const bus = new NativeEventEmitter(); } function onReady() {} bus.on('ready', onReady);",
        "import {NativeEventEmitter} from 'react-native'; let bus = new NativeEventEmitter(); bus = unknown; function onReady() {} bus.on('ready', onReady);",
        "import {NativeEventEmitter} from 'react-native'; const bus = new NativeEventEmitter(); function subscribe(bus) { bus.on('ready', onReady); } function onReady() {}",
        "import {NativeEventEmitter} from 'react-native'; for (const bus = new NativeEventEmitter(); false;) {} function onReady() {} bus.on('ready', onReady);",
        "import {NativeEventEmitter} from 'react-native'; switch (value) { case 1: const bus = new NativeEventEmitter(); } function onReady() {} bus.on('ready', onReady);",
        "import {NativeEventEmitter} from 'react-native'; const bus = new NativeEventEmitter(); delete bus.on; function onReady() {} bus.on('ready', onReady);",
    ] {
        let facts = event_generation(declaration);
        no_native_callback(&facts);
        assert!(
            facts.symbols().iter().all(|symbol| !symbol
                .qualified_name
                .contains("::react-native-event-consumer::")),
            "{declaration}"
        );
    }
    for receiver in [
        "import {NativeEventEmitter, NativeModules} from 'react-native'; const bus = new NativeEventEmitter(NativeModules.Feature);",
        "import {DeviceEventEmitter as bus} from 'react-native';",
        "import {NativeEventEmitter as Emitter} from 'react-native'; const bus = new Emitter();",
    ] {
        let facts = event_generation(&format!(
            "{receiver} function onReady() {{}} bus.on('ready', onReady);"
        ));
        let dispatcher = capability_symbol(&facts, PRODUCER_PATH, "Emitter::publish");
        let handler = capability_symbol(&facts, EVENTS_PATH, "onReady");
        let link = edge(
            &facts,
            (dispatcher, handler, native_event_calls::PROVENANCE),
        );
        assert_eq!(link.kind, EdgeKind::Calls);
        assert_eq!(
            link.confidence,
            native_bridge_details::CONVENTION_CONFIDENCE
        );
    }
}

#[test]
fn codegen_factory_requires_an_unshadowed_react_native_value_import() {
    for source in [
        "function codegenNativeComponent(name) { return name; } const label = codegenNativeComponent('Foo');".to_owned(),
        format!("{CODEGEN_IMPORT} function label(codegenNativeComponent) {{ return codegenNativeComponent('Foo'); }}"),
        "import codegenNativeComponent from './local'; const label = codegenNativeComponent('Foo');".to_owned(),
        "import type codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent'; codegenNativeComponent('Foo');".to_owned(),
        "import {type codegenNativeComponent} from 'react-native'; codegenNativeComponent('Foo');".to_owned(),
    ] {
        let facts = generation(&[("src/Foo.ts", &source), ("android/FooView.kt", "class FooView {}")]);
        assert!(facts.symbols().iter().all(|symbol| !symbol.qualified_name.contains("::fabric-component::")), "{source}");
    }
    let source = format!("{CODEGEN_IMPORT} export default codegenNativeComponent('Foo');");
    let facts = generation(&[("src/Foo.ts", &source)]);
    let component = capability_symbol(&facts, "src/Foo.ts", "src/Foo.ts::fabric-component::Foo");
    assert_eq!(component.symbol_kind, SymbolKind::Component.as_str());
}

#[test]
fn spec_argument_trivia_preserves_the_physical_native_implementation_edge() {
    for argument in ["< Spec >", "< /* type */ Spec /* end */ >"] {
        let source = format!(
            "interface Spec extends TurboModule {{ run(): void; }} export default TurboModuleRegistry.getEnforcing{argument}('Feature');"
        );
        let facts = generation(&[
            ("src/NativeFeature.ts", &source),
            (
                "android/Feature.kt",
                "class FeatureModule { @ReactMethod fun run() {} }",
            ),
        ]);
        let spec = capability_symbol(
            &facts,
            "src/NativeFeature.ts",
            "src/NativeFeature.ts::turbo-module-spec-method::Feature::run",
        );
        let implementation = capability_symbol(&facts, "android/Feature.kt", "FeatureModule::run");
        let link = edge(
            &facts,
            (spec, implementation, TURBO_NATIVE_BRIDGE_PROVENANCE),
        );
        assert_eq!(link.kind, EdgeKind::References);
        assert_eq!(link.confidence, FRAMEWORK_CONVENTION_CONFIDENCE);
    }
    for source in [
        "interface Spec extends TurboModule { run(): void; } export default TurboModuleRegistry.getEnforcing<Spec, Other>('Feature');",
        "interface Spec extends TurboModule { run(): void; } class Container<Spec> { value = TurboModuleRegistry.get<Spec>('Wrong'); }",
    ] {
        let facts = generation(&[("src/NativeFeature.ts", source)]);
        assert!(
            facts.symbols().iter().all(|symbol| !symbol
                .qualified_name
                .contains("::turbo-module-spec-method::")),
            "{source}"
        );
    }
}

#[test]
fn ordinary_native_exports_named_remove_preserve_the_physical_endpoint() {
    let facts = generation(&[
        (
            "android/FileModule.java",
            "class FileModule { @ReactMethod public void remove(String path) {} public void hidden() {} }",
        ),
        (
            "src/use.js",
            "NativeModules.File.remove('document'); NativeModules.File.hidden();",
        ),
    ]);
    capability_symbol(
        &facts,
        "android/FileModule.java",
        "android/FileModule.java::react-native-method::File::remove",
    );
    let method = capability_symbol(&facts, "android/FileModule.java", "FileModule::remove");
    let reference = call(&facts, ("src/use.js", "remove"));
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&method.symbol_id),
        "{:?}",
        facts.references()
    );
    assert_eq!(
        reference.resolution_provenance,
        native_bridge_details::PHYSICAL_METHOD_PROVENANCE
    );
    assert_eq!(
        reference.confidence,
        native_bridge_details::CONVENTION_CONFIDENCE
    );
    assert!(
        call(&facts, ("src/use.js", "hidden"))
            .target_symbol_id
            .is_none()
    );
}
