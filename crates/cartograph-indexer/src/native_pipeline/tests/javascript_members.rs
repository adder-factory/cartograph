use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, DYNAMIC_DISPATCH_PROVENANCE,
    DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE, EXACT_SAME_FILE_PROVENANCE, EdgeKind,
    FRAMEWORK_CONVENTION_PROVENANCE, IMPORT_BINDING_PROVENANCE,
    JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE, ReferenceKind, build_capability_generation,
    capability_file_symbol, capability_symbol, native_bridge_details,
};

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reverse = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reverse.digest());
    assert_eq!(forward.references(), reverse.references());
    assert_eq!(forward.edges(), reverse.edges());
    forward
}

#[test]
fn string_keyed_value_calls_keep_base_dynamic_function_dispatch() {
    let facts = generation(&[
        (
            "src/service.ts",
            "export function renderPanel(): string { return 'panel'; }\n",
        ),
        (
            "src/build.ts",
            "export function loadRenderer(mod: Record<string, () => string>): string { return mod[\"renderPanel\"](); }\nexport function singleQuoted(mod: Record<string, () => string>): string { return mod['renderPanel'](); }\nexport function templateQuoted(mod: Record<string, () => string>): string { return mod[`renderPanel`](); }\n",
        ),
    ]);
    let target = capability_symbol(&facts, "src/service.ts", "renderPanel");
    for name in ["loadRenderer", "singleQuoted", "templateQuoted"] {
        let owner = capability_symbol(&facts, "src/build.ts", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("renderPanel", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
        assert!((reference.confidence - 0.65).abs() < f32::EPSILON);
        assert!(facts.edges().iter().any(|edge| {
            edge.source_symbol_id == owner.symbol_id
                && edge.target_symbol_id == target.symbol_id
                && edge.kind == EdgeKind::Calls
                && edge.provenance == DYNAMIC_DISPATCH_PROVENANCE
                && (edge.confidence - 0.65).abs() < f32::EPSILON
        }));
    }
}

#[test]
fn string_keyed_value_calls_abstain_on_ambiguous_or_private_functions() {
    for second in [
        "export function renderPanel(): string { return 'other'; }\n",
        "function privatePanel(): string { return 'private'; }\n",
    ] {
        let facts = generation(&[
            (
                "src/service.ts",
                "export function renderPanel(): string { return 'panel'; }\n",
            ),
            ("src/other.ts", second),
            (
                "src/build.ts",
                "export function loadRenderer(mod: Record<string, () => string>): string { mod[\"privatePanel\"](); return mod[\"renderPanel\"](); }\n",
            ),
        ]);
        let owner = capability_symbol(&facts, "src/build.ts", "loadRenderer");
        let name = if second.starts_with("export ") {
            "renderPanel"
        } else {
            "privatePanel"
        };
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None);
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn string_keyed_intrinsic_calls_abstain_when_receiver_scope_is_uncertain() {
    let facts = generation(&[
        ("src/service.js", "export function log() {}\n"),
        (
            "src/caller.js",
            "function run() { with ({}) { console['log']('x'); } }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/caller.js", "run");
    let reference = CapabilityReferenceQuery::new(&facts, owner).named("log", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

#[test]
fn expo_bridge_member_calls_keep_explicit_native_resolution() {
    let facts = generation(&[
        (
            "js/settings.ts",
            "import { requireNativeModule } from 'expo-modules-core';\nconst ExpoSettings = requireNativeModule('ExpoSettings');\nexport async function applyTheme() { ExpoSettings.getTheme(); ExpoSettings.spaced(); ExpoSettings.plainMethod(); return ExpoSettings.setTheme('light'); }\n",
        ),
        (
            "android/SettingsModule.kt",
            "import expo.modules.kotlin.modules.Module\nimport expo.modules.kotlin.modules.ModuleDefinition\nclass SettingsModule : Module() { fun plainMethod() {} override fun definition() = ModuleDefinition { Name(\"ExpoSettings\"); Function(\"getTheme\") { \"dark\" }; Function (\"spaced\") { 1 }; AsyncFunction(\"setTheme\") { theme: String -> theme.uppercase() } } }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "js/settings.ts", "applyTheme");
    for method in ["getTheme", "spaced", "setTheme"] {
        assert_native_bridge_member(
            &facts,
            (owner, method),
            (
                "android/SettingsModule.kt",
                &format!("android/SettingsModule.kt::expo-module-method::ExpoSettings::{method}"),
                native_bridge_details::ALIAS_PROVENANCE,
                native_bridge_details::CONVENTION_CONFIDENCE,
            ),
        );
    }
    assert_unexported_bridge_member(&facts, owner);
}

#[test]
fn react_native_destructured_and_qualified_members_keep_native_resolution() {
    let facts = generation(&[
        (
            "src/native.js",
            "import { NativeModules } from 'react-native';\nconst { RNThing } = NativeModules;\nexport async function loadThing() { const value = await RNThing.doSomething('a'); const other = await NativeModules.RNThing.getThing(); await RNThing.plainMethod(); return [value, other]; }\n",
        ),
        (
            "ios/RN/RNThing.m",
            "#import <React/RCTBridgeModule.h>\n@implementation RNThing\nRCT_EXPORT_MODULE()\nRCT_EXPORT_METHOD(doSomething:(NSString *)name) {}\nRCT_REMAP_METHOD(getThing, getThingWithResolver:(RCTPromiseResolveBlock)resolve) {}\n- (void)plainMethod {}\n@end\n",
        ),
    ]);
    for (owner, method) in [
        ("loadThing::value", "doSomething"),
        ("loadThing::other", "getThing"),
    ] {
        let owner = capability_symbol(&facts, "src/native.js", owner);
        let (provenance, confidence) = if method == "doSomething" {
            (
                native_bridge_details::ALIAS_PROVENANCE,
                native_bridge_details::CONVENTION_CONFIDENCE,
            )
        } else {
            (
                DYNAMIC_DISPATCH_PROVENANCE,
                super::DYNAMIC_DISPATCH_CONFIDENCE,
            )
        };
        assert_native_bridge_member(
            &facts,
            (owner, method),
            (
                "ios/RN/RNThing.m",
                &format!("ios/RN/RNThing.m::react-native-method::RNThing::{method}"),
                provenance,
                confidence,
            ),
        );
    }
    let owner = capability_symbol(&facts, "src/native.js", "loadThing");
    assert_unexported_bridge_member(&facts, owner);
}

fn assert_unexported_bridge_member(
    facts: &CanonicalGenerationFacts,
    owner: &super::super::SymbolInput,
) {
    let reference =
        CapabilityReferenceQuery::new(facts, owner).named("plainMethod", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

fn assert_native_bridge_member(
    facts: &CanonicalGenerationFacts,
    (owner, name): (&super::super::SymbolInput, &str),
    (path, qualified, provenance, confidence): (&str, &str, &str, f32),
) {
    let target = capability_symbol(facts, path, qualified);
    let references = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                && reference.reference_name == name
                && reference.reference_kind == ReferenceKind::Calls.as_str()
                && reference.resolution_provenance == provenance
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 1, "{name}: {:?}", facts.references());
    assert_eq!(
        references[0].target_symbol_id.as_ref(),
        Some(&target.symbol_id)
    );
    assert_eq!(references[0].confidence, confidence);
}

const EXPO_COLLISION_MODULE: (&str, &str) = (
    "android/SettingsModule.kt",
    "import expo.modules.kotlin.modules.Module\nimport expo.modules.kotlin.modules.ModuleDefinition\nclass SettingsModule : Module() { override fun definition() = ModuleDefinition { Name(\"ExpoSettings\"); Function(\"map\") { 1 }; Function(\"set\") { 1 }; Function(\"getTheme\") { \"dark\" } } }\n",
);

#[test]
fn native_bridge_homonyms_cannot_capture_builtin_receivers() {
    let facts = generation(&[
        EXPO_COLLISION_MODULE,
        (
            "src/use.ts",
            "export function use() { return [1, 2].map(x => x); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let reference = CapabilityReferenceQuery::new(&facts, owner).named("map", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

#[test]
fn native_bridge_homonyms_cannot_capture_shadowed_import_receivers() {
    let facts = generation(&[
        EXPO_COLLISION_MODULE,
        ("src/cache.ts", "export class Cache { static set() {} }\n"),
        (
            "src/use.ts",
            "import { Cache } from './cache';\nexport function use(Cache: { set(): void }) { Cache.set(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    for name in ["Cache.set", "set"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None);
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn named_import_member_ownership_preserves_receiver_and_complete_path_evidence() {
    for path in ["src/use.js", "src/use.jsx", "src/use.ts", "src/use.tsx"] {
        let facts = generation(&[
            (
                "src/cache.js",
                "export class Child { static set() {} } export class Cache { static child = Child; static set() {} }",
            ),
            (
                path,
                "import { Cache as Builder } from './cache'; export function direct() { Builder.set(); } export function shadowed(Builder) { Builder.set(); } export function chained() { Builder.child.set(); } export function missing() { Builder.missing(); }",
            ),
        ]);
        let owner = capability_symbol(&facts, path, "direct");
        let target = capability_symbol(&facts, "src/cache.js", "Cache::set");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("Builder.set", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        for (owner, name) in [
            ("shadowed", "Builder.set"),
            ("chained", "Builder.child.set"),
            ("missing", "Builder.missing"),
        ] {
            let owner = capability_symbol(&facts, path, owner);
            let reference =
                CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
            assert_eq!(reference.target_symbol_id, None, "{path}: {reference:?}");
            assert_eq!(
                reference.resolution_provenance,
                DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
            );
        }
    }
}

#[test]
fn native_bridge_homonyms_keep_exact_namespace_import_targets() {
    let facts = generation(&[
        EXPO_COLLISION_MODULE,
        ("src/tools.ts", "export function getTheme() {}\n"),
        (
            "src/use.ts",
            "import * as tools from './tools';\nexport function use() { tools.getTheme(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let target = capability_symbol(&facts, "src/tools.ts", "getTheme");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("getTheme", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    assert!((reference.confidence - 0.65).abs() < f32::EPSILON);
}

#[test]
fn builtin_member_calls_never_bind_to_project_homonyms() {
    let facts = generation(&[
        (
            "src/homonyms.ts",
            "export class Arr { public map() {} public set() {} public trim() {} public log() {} public stringify() {} }\nexport function log() {}\n",
        ),
        (
            "src/caller.ts",
            "export function array() { return [1, 2].map(x => x); }\nexport function collection() { new Map().set('a', 1); }\nexport function text(value: string) { return value.trim(); }\nexport function consoleCall() { console.log('x'); }\nexport function jsonCall() { JSON.stringify({}); }\n",
        ),
    ]);
    for (owner, member, provenance) in [
        ("array", "map", DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
        ("collection", "set", DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
        ("text", "trim", DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
        (
            "consoleCall",
            "log",
            JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE,
        ),
        (
            "jsonCall",
            "stringify",
            JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE,
        ),
    ] {
        let owner = capability_symbol(&facts, "src/caller.ts", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{member}: {reference:?}");
        assert_eq!(reference.resolution_provenance, provenance);
    }
}

#[test]
fn builtin_names_keep_lexical_and_exact_import_backing() {
    let facts = generation(&[
        (
            "src/tools.ts",
            "export function map() {}\nexport class Cache { static set() {} }\n",
        ),
        (
            "src/use.ts",
            "import * as tools from './tools';\nimport { map as transform } from './tools';\nexport function use() { tools.map(); transform(); }\nfunction map() {}\nexport function local() { map(); }\n",
        ),
    ]);
    let use_ = capability_symbol(&facts, "src/use.ts", "use");
    let imported = capability_symbol(&facts, "src/tools.ts", "map");
    let reference = CapabilityReferenceQuery::new(&facts, use_).named("map", ReferenceKind::Calls);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&imported.symbol_id)
    );
    assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    let alias =
        CapabilityReferenceQuery::new(&facts, use_).named("transform", ReferenceKind::Calls);
    assert_eq!(alias.target_symbol_id.as_ref(), Some(&imported.symbol_id));
    assert_eq!(alias.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    let local = capability_symbol(&facts, "src/use.ts", "local");
    let map = capability_symbol(&facts, "src/use.ts", "map");
    let reference = CapabilityReferenceQuery::new(&facts, local).named("map", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&map.symbol_id));
    assert_eq!(reference.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
}

#[test]
fn intrinsic_receiver_syntax_variants_never_bind_project_methods() {
    let facts = generation(&[
        (
            "src/logger.ts",
            "export class Logger { log() {} invalidate() {} }\n",
        ),
        (
            "src/use.ts",
            "import Remote from 'external';\nexport function optional() { console?.log('x'); }\nexport function comment() { console /*comment*/ . log('x'); }\nexport function lineComment() { console //comment\n . log('x'); }\nexport function computed() { console['log']('x'); }\nexport function complex() { new Remote().invalidate(); }\n",
        ),
    ]);
    for (owner, member, provenance) in [
        (
            "optional",
            "log",
            JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE,
        ),
        ("comment", "log", JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE),
        (
            "lineComment",
            "log",
            JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE,
        ),
        ("computed", "log", DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
        (
            "complex",
            "invalidate",
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ),
    ] {
        let owner = capability_symbol(&facts, "src/use.ts", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{reference:?}");
        assert_eq!(reference.resolution_provenance, provenance);
    }
}

#[test]
fn normalized_receivers_keep_imports_but_function_alias_members_abstain() {
    let facts = generation(&[
        (
            "src/tools.ts",
            "export function log() {}\nexport default function write() {}\nexport class Cache { static set() {} }\n",
        ),
        (
            "src/use.ts",
            "import * as tools from './tools';\nimport logger, { log as named, Cache } from './tools';\nexport function optional() { tools?.log(); }\nexport function commented() { tools /*comment*/ . log(); }\nexport function staticMember() { Cache /*comment*/ . set(); }\nexport function defaultMember() { tools.default(); }\nexport function defaultAlias() { logger.write(); }\nexport function namedAlias() { named.log(); }\n",
        ),
    ]);
    for (owner, member, target) in [
        ("optional", "log", "log"),
        ("commented", "log", "log"),
        ("staticMember", "set", "Cache::set"),
        ("defaultMember", "default", "write"),
    ] {
        let owner = capability_symbol(&facts, "src/use.ts", owner);
        let target = capability_symbol(&facts, "src/tools.ts", target);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    }
    for (owner, member) in [("defaultAlias", "write"), ("namedAlias", "log")] {
        let owner = capability_symbol(&facts, "src/use.ts", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{reference:?}");
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
        let full_name = if member == "write" {
            "logger.write"
        } else {
            "named.log"
        };
        let qualified =
            CapabilityReferenceQuery::new(&facts, owner).named(full_name, ReferenceKind::Calls);
        assert_eq!(qualified.target_symbol_id, None, "{qualified:?}");
        assert_eq!(
            qualified.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
    let owner = capability_symbol(&facts, "src/use.ts", "defaultMember");
    let target = capability_symbol(&facts, "src/tools.ts", "write");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("tools.default", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
}

#[test]
fn implicit_public_methods_of_exported_classes_are_dynamic_targets() {
    let facts = generation(&[
        (
            "src/cache.ts",
            "export class EmbeddingCache { invalidate() {} private secret() {} protected internal() {} #hidden() {} }\nclass Local { private unreachable() {} }\nexport function log() {}\n",
        ),
        (
            "src/use.ts",
            "export function use(cache: unknown) { cache.invalidate(); cache.secret(); cache.internal(); cache.unreachable(); cache.log(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let invalidate = capability_symbol(&facts, "src/cache.ts", "EmbeddingCache::invalidate");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("invalidate", ReferenceKind::Calls);
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&invalidate.symbol_id)
    );
    assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    assert!((reference.confidence - 0.65).abs() < f32::EPSILON);
    for name in ["secret", "internal", "unreachable", "log"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{name}: {reference:?}");
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn implicit_public_method_names_abstain_when_two_classes_declare_them() {
    let facts = generation(&[
        ("src/a.ts", "export class A { run() {} }\n"),
        ("src/b.ts", "export class B { run() {} }\n"),
        (
            "src/use.ts",
            "export function use(value: unknown) { value.run(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let reference = CapabilityReferenceQuery::new(&facts, owner).named("run", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

const EXPRESS_MANIFEST: &str = r#"{"name":"app","dependencies":{"express":"4.18.0"}}"#;

#[test]
fn shadowed_import_receivers_never_refine_to_project_static_members() {
    let facts = generation(&[
        ("src/cache.ts", "export class Cache { static set() {} }\n"),
        (
            "src/use.ts",
            "import { Cache } from './cache';\nexport function parameter(Cache: { set(): void }) { Cache.set(); }\nexport function local() { const Cache = { set() {} }; Cache.set(); }\nexport function caught() { try {} catch (Cache) { Cache.set(); } }\nexport function destructured({ Cache }: { Cache: { set(): void } }) { Cache.set(); }\nexport function outer(Cache: { set(): void }) { function inner() { Cache.set(); } inner(); }\nexport function plain() { Cache.set(); }\n",
        ),
    ]);
    for name in [
        "parameter",
        "local",
        "caught",
        "destructured",
        "outer::inner",
    ] {
        let owner = capability_symbol(&facts, "src/use.ts", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("set", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{name}: {reference:?}");
    }
    let owner = capability_symbol(&facts, "src/use.ts", "plain");
    let target = capability_symbol(&facts, "src/cache.ts", "Cache::set");
    let reference = CapabilityReferenceQuery::new(&facts, owner).named("set", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
}

#[test]
fn framework_member_transforms_never_capture_local_receiver_bindings() {
    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        ("src/mail.ts", "export function send() {}\n"),
        (
            "src/routes.ts",
            "export function parameter(MailService: { send(): void }) { MailService.send(); }\nexport function local() { const MailService = { send() {} }; MailService.send(); }\nexport function caught() { try {} catch (MailService) { MailService.send(); } }\nexport function destructured({ MailService }: { MailService: { send(): void } }) { MailService.send(); }\nexport function plain() { MailService.send(); }\n",
        ),
    ]);
    for name in ["parameter", "local", "caught", "destructured"] {
        let owner = capability_symbol(&facts, "src/routes.ts", name);
        let reference = CapabilityReferenceQuery::new(&facts, owner)
            .named("MailService.send", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{name}: {reference:?}");
    }
    let owner = capability_symbol(&facts, "src/routes.ts", "plain");
    let target = capability_symbol(&facts, "src/mail.ts", "send");
    let reference = CapabilityReferenceQuery::new(&facts, owner)
        .named("MailService.send", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        FRAMEWORK_CONVENTION_PROVENANCE
    );
}

#[test]
fn implicit_public_methods_keep_explicit_public_same_file_exclusion() {
    for modifier in ["", "public "] {
        let source = format!(
            "export class Cache {{ {modifier}invalidate() {{}} }} export function use() {{ const other = {{ invalidate: () => {{}} }}; other.invalidate(); }}"
        );
        let facts = generation(&[("src/example.ts", &source)]);
        let owner = capability_symbol(&facts, "src/example.ts", "use");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("invalidate", ReferenceKind::Calls);
        assert_eq!(
            reference.target_symbol_id, None,
            "{modifier}: {reference:?}"
        );
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn constructor_member_calls_have_explicit_receiver_proof() {
    for modifier in ["public ", ""] {
        let source = format!("export class Cache {{ {modifier}invalidate() {{}} }}");
        let facts = generation(&[
            ("src/cache.ts", &source),
            (
                "src/use.ts",
                "import { Cache } from './cache'; export function use() { new Cache().invalidate(); }\n",
            ),
        ]);
        let owner = capability_symbol(&facts, "src/use.ts", "use");
        let target = capability_symbol(&facts, "src/cache.ts", "Cache::invalidate");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("invalidate", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(
            reference.resolution_provenance,
            "native-explicit-receiver-type"
        );
        assert!((reference.confidence - 0.95).abs() < f32::EPSILON);
    }
}

#[test]
fn constructor_identity_does_not_back_members_of_a_child_or_shadowed_constructor() {
    let facts = generation(&[
        (
            "src/cache.ts",
            "export class Cache { public invalidate() {} }\n",
        ),
        (
            "src/use.ts",
            "import { Cache } from './cache';\nexport function child() { new Cache().child.invalidate(); }\nexport function computed(key: string) { new Cache()[key].invalidate(); }\nexport function shadowed(Cache: any) { new Cache().invalidate(); }\n",
        ),
    ]);
    for name in ["child", "computed", "shadowed"] {
        let owner = capability_symbol(&facts, "src/use.ts", name);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("invalidate", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{name}: {reference:?}");
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn parenthesized_member_refinements_respect_lexical_shadowing() {
    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        ("src/cache.ts", "export class Cache { static set() {} }\n"),
        ("src/mail.ts", "export function send() {}\n"),
        (
            "src/use.ts",
            "import { Cache } from './cache';\nexport function cache(Cache: { set(): void }) { (Cache.set)(); }\nexport function mail(MailService: { send(): void }) { (MailService.send)(); }\nexport function plain() { (Cache.set)(); (MailService.send)(); }\n",
        ),
    ]);
    for (owner, member) in [("cache", "Cache.set"), ("mail", "MailService.send")] {
        let owner = capability_symbol(&facts, "src/use.ts", owner);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{member}: {reference:?}");
    }
    let owner = capability_symbol(&facts, "src/use.ts", "plain");
    for (member, path, target, provenance) in [
        (
            "Cache.set",
            "src/cache.ts",
            "Cache::set",
            IMPORT_BINDING_PROVENANCE,
        ),
        (
            "MailService.send",
            "src/mail.ts",
            "send",
            FRAMEWORK_CONVENTION_PROVENANCE,
        ),
    ] {
        let target = capability_symbol(&facts, path, target);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, provenance);
    }
}

#[test]
fn local_imports_keep_qualified_edges_without_dynamic_member_guesses() {
    let facts = generation(&[
        ("src/tools.ts", "export function qux() {}\n"),
        (
            "src/other.ts",
            "export class Logger { public log() {} invalidate() {} }\n",
        ),
        (
            "src/use.ts",
            "export async function use() { const tools = await import('./tools'); tools.qux(); tools.log(); tools.invalidate(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let target = capability_symbol(&facts, "src/tools.ts", "qux");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("tools.qux", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    for member in ["log", "invalidate"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(member, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{member}: {reference:?}");
        assert_eq!(
            reference.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn default_public_methods_match_explicit_public_unexported_class_eligibility() {
    for modifier in ["", "public "] {
        let source = format!("class Cache {{ {modifier}invalidate() {{}} }}\n");
        let facts = generation(&[
            ("src/cache.ts", &source),
            (
                "src/use.ts",
                "export function use(value: unknown) { value.invalidate(); }\n",
            ),
        ]);
        let owner = capability_symbol(&facts, "src/use.ts", "use");
        let target = capability_symbol(&facts, "src/cache.ts", "Cache::invalidate");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("invalidate", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
        assert!((reference.confidence - 0.65).abs() < f32::EPSILON);
    }
}

#[test]
fn imported_static_resolution_work_stays_linear_with_same_name_classes() {
    let small = static_member_resolution_work(64);
    let large = static_member_resolution_work(128);
    assert!(
        large <= small * 2 + 32,
        "nonlinear candidate work: {small} -> {large}"
    );
    assert!(large <= 128 * 40, "unbounded per-call work: {large}");
}

fn static_member_resolution_work(size: usize) -> u64 {
    use super::{
        ExtractedReferenceQuery, FileDocumentIdentity, FileImportBindingIndex,
        FileResolutionContext, ImportBindingScratch, ResolutionIndexContext, ResolveBudget,
        TEST_GENERATION_BYTES, build_resolution_index, resolve_extracted_reference,
        test_source_root,
    };
    let extracted = static_work_fixture(size);
    let mut budget =
        ResolveBudget::new(0, TEST_GENERATION_BYTES).unwrap_or_else(|_| panic!("budget"));
    let index = build_resolution_index(
        &extracted,
        ResolutionIndexContext {
            source_root: &test_source_root(),
            budget: &mut budget,
            cancelled: &mut || false,
        },
    )
    .unwrap_or_else(|_| panic!("index"));
    let file = extracted
        .files
        .iter()
        .find(|file| file.file.normalized_path == "src/use.ts")
        .unwrap_or_else(|| panic!("use file"));
    let identity = FileDocumentIdentity {
        file_id: file.file.file_id.clone(),
        path: file.file.normalized_path.clone(),
        language: file.file.language.clone(),
    };
    let imports =
        FileImportBindingIndex::new(&file.import_bindings, &mut budget, &identity.language)
            .unwrap_or_else(|_| panic!("imports"));
    let mut scratch = ImportBindingScratch::new(file.import_bindings.len(), &mut budget)
        .unwrap_or_else(|_| panic!("scratch"));
    let receivers = crate::native_pipeline::generic_resolution::ReceiverSites::default();
    let lookups = crate::native_pipeline::receiver_resolution::FileLookups::new(
        &[],
        &mut budget,
        &mut || false,
    )
    .unwrap_or_else(|_| panic!("receiver lookups"));
    let context = FileResolutionContext {
        current_receivers: &receivers,
        receiver_lookups: &lookups,
        identity: &identity,
        file_symbol_id: index
            .file_symbols
            .get(&identity.file_id)
            .unwrap_or_else(|| panic!("file symbol")),
        import_bindings: &imports,
    };
    let target = index
        .candidates
        .get("C0::run")
        .and_then(|bucket| bucket.as_slice().first())
        .unwrap_or_else(|| panic!("target"));
    let mut polls = 0;
    for reference in file
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
    {
        let resolution = resolve_extracted_reference(
            &index,
            ExtractedReferenceQuery {
                context: &context,
                reference,
                import_binding_scratch: &mut scratch,
            },
            &mut || {
                polls += 1;
                false
            },
        )
        .unwrap_or_else(|_| panic!("resolve"));
        let resolved = resolution
            .target
            .unwrap_or_else(|| panic!("unresolved call"));
        assert_eq!(resolved.symbol_id, target.symbol_id);
        let provenance = if reference.name == "run" {
            DYNAMIC_DISPATCH_PROVENANCE
        } else {
            IMPORT_BINDING_PROVENANCE
        };
        assert_eq!(resolved.provenance, provenance);
    }
    polls
}

fn static_work_fixture(size: usize) -> super::NativeFactAccumulator {
    use super::{
        NativeExtractor, NativeFactAccumulator, SourceLimits, TEST_GENERATION_BYTES,
        TEST_SOURCE_BYTES, append_fixture_text,
    };
    let mut classes = String::new();
    for index in 0..size {
        append_fixture_text(
            &mut classes,
            format_args!("export class C{index} {{ static run() {{}} }}\n"),
        );
    }
    let caller = format!(
        "import {{ C0 }} from './classes'; export function use() {{ {} }}\n",
        "C0.run();".repeat(size)
    );
    let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
    let limits = SourceLimits::new(TEST_SOURCE_BYTES).unwrap_or_else(|_| panic!("limits"));
    for (path, source) in [("src/classes.ts", classes), ("src/use.ts", caller)] {
        let snapshot =
            cartograph_extract::SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
                .unwrap_or_else(|_| panic!("snapshot"));
        let extracted = NativeExtractor::new(snapshot.language())
            .and_then(|mut extractor| extractor.extract(&snapshot))
            .unwrap_or_else(|_| panic!("extract"));
        accumulator
            .push(extracted)
            .unwrap_or_else(|_| panic!("accumulate"));
    }
    accumulator
}

#[test]
fn detected_express_middleware_transforms_names_and_preserves_exact_names() {
    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        (
            "src/middleware/auth.ts",
            "export function auth() {}\nexport function ValidateBody() {}\n",
        ),
        ("src/util.ts", "export function auth() {}\n"),
        (
            "src/routes.ts",
            "export function wire() { authMiddleware(); authmiddleware(); validatebody(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    for (name, target) in [
        ("authMiddleware", "auth"),
        ("authmiddleware", "auth"),
        ("validatebody", "ValidateBody"),
    ] {
        let target = capability_symbol(&facts, "src/middleware/auth.ts", target);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{name}"
        );
        assert_eq!(
            reference.resolution_provenance,
            FRAMEWORK_CONVENTION_PROVENANCE
        );
        assert!((reference.confidence - 0.85).abs() < f32::EPSILON);
    }
}

#[test]
fn express_member_transforms_use_paths_or_one_controller_class_file() {
    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        ("src/controllers/USER.ts", "export function getUser() {}\n"),
        ("src/mail.ts", "export function send() {}\n"),
        (
            "src/handlers.ts",
            "export class TaskController {}\nexport function handle() {}\n",
        ),
        (
            "src/other.ts",
            "export function getUser() {}\nexport function send() {}\nexport function handle() {}\n",
        ),
        (
            "src/routes.ts",
            "export function wire() { UserController.getUser(); MailService.send(); MailHelper.send(); MailUtil.send(); MailUtils.send(); TaskController.handle(); GhostController.getUser(); MissingService.send(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    for (name, path, target) in [
        (
            "UserController.getUser",
            "src/controllers/USER.ts",
            "getUser",
        ),
        ("MailService.send", "src/mail.ts", "send"),
        ("MailHelper.send", "src/mail.ts", "send"),
        ("MailUtil.send", "src/mail.ts", "send"),
        ("MailUtils.send", "src/mail.ts", "send"),
        ("TaskController.handle", "src/handlers.ts", "handle"),
    ] {
        let target = capability_symbol(&facts, path, target);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{name}: {reference:?}"
        );
        assert_eq!(
            reference.resolution_provenance,
            FRAMEWORK_CONVENTION_PROVENANCE
        );
    }
    for name in ["GhostController.getUser", "MissingService.send"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None);
    }
}

#[test]
fn framework_name_transforms_require_detection_and_abstain_on_ambiguity() {
    for manifest in [r"{}", EXPRESS_MANIFEST] {
        let facts = generation(&[
            ("package.json", manifest),
            ("src/middleware/a.ts", "export function auth() {}\n"),
            ("src/middleware/b.ts", "export function auth() {}\n"),
            ("src/mail/a.ts", "export function send() {}\n"),
            ("src/mail/b.ts", "export function send() {}\n"),
            (
                "src/routes.ts",
                "export function wire() { authMiddleware(); MailService.send(); }\n",
            ),
        ]);
        let owner = capability_symbol(&facts, "src/routes.ts", "wire");
        for name in ["authMiddleware", "MailService.send"] {
            let reference =
                CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
            assert_eq!(reference.target_symbol_id, None);
        }
    }
    let facts = generation(&[
        ("src/middleware/auth.ts", "export function auth() {}\n"),
        (
            "src/routes.ts",
            "export function wire() { authMiddleware(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    assert_eq!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("authMiddleware", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}

#[test]
fn react_context_and_provider_transforms_bind_unique_base_names() {
    let facts = generation(&[
        (
            "package.json",
            r#"{"name":"app","dependencies":{"react":"19.0.0"}}"#,
        ),
        ("src/context/theme.ts", "export function Theme() {}\n"),
        (
            "src/app.ts",
            "export function App() { ThemeContext(); ThemeProvider(); MissingContext(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/app.ts", "App");
    let target = capability_symbol(&facts, "src/context/theme.ts", "Theme");
    for name in ["ThemeContext", "ThemeProvider"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(
            reference.resolution_provenance,
            FRAMEWORK_CONVENTION_PROVENANCE
        );
    }
    assert_eq!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("MissingContext", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}

#[test]
fn imported_classes_back_builtin_static_members_but_external_receivers_abstain() {
    let facts = generation(&[
        (
            "src/cache.ts",
            "export class Cache { static set() {} private static get() {} clear() {} }\nexport class Local { log() {} useState() {} }\n",
        ),
        (
            "src/use.ts",
            "import { Cache } from './cache';\nimport React from 'react';\nimport logger from 'external-logger';\nexport function use() { Cache.set(); Cache.get(); Cache.clear(); React.useState(); logger.log(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let target = capability_symbol(&facts, "src/cache.ts", "Cache::set");
    let set = CapabilityReferenceQuery::new(&facts, owner).named("set", ReferenceKind::Calls);
    assert_eq!(set.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(set.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    let qualified =
        CapabilityReferenceQuery::new(&facts, owner).named("Cache.set", ReferenceKind::Calls);
    assert_eq!(qualified.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(qualified.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    for name in ["get", "clear", "useState", "log"] {
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id, None, "{name}: {reference:?}");
    }
    for name in ["useState", "log"] {
        assert_eq!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named(name, ReferenceKind::Calls)
                .resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
}

#[test]
fn a_same_name_import_never_backs_a_builtin_on_another_receiver() {
    let facts = generation(&[
        ("src/tools.ts", "export function map() {}\n"),
        (
            "src/use.ts",
            "import { map } from './tools';\nexport function use() { [1, 2].map(x => x); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let reference = CapabilityReferenceQuery::new(&facts, owner).named("map", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

#[test]
fn opaque_imported_values_keep_their_existing_consumer_edge() {
    let facts = generation(&[
        (
            "src/schema.ts",
            "import { z } from 'zod';\nexport const RuntimeSchema = z.object({});\nexport const VERSION = 1;\nexport class Other { safeParse() {} }\n",
        ),
        (
            "src/use.ts",
            "import { RuntimeSchema } from './schema';\nimport * as schema from './schema';\nexport function use(value: unknown) { RuntimeSchema.safeParse(value); }\nexport function invalid() { schema.VERSION(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    let target = facts
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == "RuntimeSchema" && symbol.symbol_kind == "constant")
        .unwrap_or_else(|| panic!("missing RuntimeSchema value declaration"));
    let qualified = CapabilityReferenceQuery::new(&facts, owner)
        .named("RuntimeSchema.safeParse", ReferenceKind::Calls);
    assert_eq!(qualified.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(qualified.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    let dynamic =
        CapabilityReferenceQuery::new(&facts, owner).named("safeParse", ReferenceKind::Calls);
    assert_eq!(dynamic.target_symbol_id, None);
    assert_eq!(
        dynamic.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
    let owner = capability_symbol(&facts, "src/use.ts", "invalid");
    let dynamic =
        CapabilityReferenceQuery::new(&facts, owner).named("VERSION", ReferenceKind::Calls);
    assert_eq!(dynamic.target_symbol_id, None);
    assert_eq!(
        dynamic.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

#[test]
fn unbacked_bare_builtin_names_cannot_bind_in_another_file() {
    let facts = generation(&[
        ("src/tools.ts", "export function map() {}\n"),
        ("src/use.ts", "export function use() { map(); }\n"),
    ]);
    let owner = capability_symbol(&facts, "src/use.ts", "use");
    assert_eq!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("map", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}

#[test]
fn dispatch_tables_keep_exact_value_backing_for_builtin_named_handlers() {
    let facts = generation(&[(
        "src/table.ts",
        "function map() {}\nconst HANDLERS = { start: map };\nexport function dispatch(kind: string) { HANDLERS[kind]?.(); }\nexport function array() { [1].map(x => x); }\n",
    )]);
    let target = capability_symbol(&facts, "src/table.ts", "map");
    let file = capability_file_symbol(&facts, "src/table.ts");
    let bound = CapabilityReferenceQuery::new(&facts, file).named("map", ReferenceKind::Calls);
    assert_eq!(bound.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(bound.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    let array_owner = capability_symbol(&facts, "src/table.ts", "array");
    let array =
        CapabilityReferenceQuery::new(&facts, array_owner).named("map", ReferenceKind::Calls);
    assert_eq!(array.target_symbol_id, None);
    assert_eq!(
        array.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );
}

#[test]
fn builtin_named_dispatch_handlers_abstain_without_a_callable_value_binding() {
    let facts = generation(&[(
        "src/table.ts",
        "function map() {}\nexport function outer(kind: string) { function map() {} const HANDLERS = { start: map }; HANDLERS[kind]?.(); }\n",
    )]);
    let file = capability_file_symbol(&facts, "src/table.ts");
    let reference = CapabilityReferenceQuery::new(&facts, file).named("map", ReferenceKind::Calls);
    // This nested initializer has no per-file value reference to bind its
    // dispatch target. Neither the local nor the outer namesake is proven.
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );

    let facts = generation(&[
        (
            "src/table.ts",
            "const HANDLERS = { start: map }; export function dispatch(kind: string) { HANDLERS[kind]?.(); }\n",
        ),
        ("src/other.ts", "export function map() {}\n"),
    ]);
    let file = capability_file_symbol(&facts, "src/table.ts");
    let reference = CapabilityReferenceQuery::new(&facts, file).named("map", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id, None);
    assert_eq!(
        reference.resolution_provenance,
        DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
    );

    let facts = generation(&[(
        "src/table.ts",
        "const map = 1; const HANDLERS = { start: map }; export function dispatch(kind: string) { HANDLERS[kind]?.(); }\n",
    )]);
    let file = capability_file_symbol(&facts, "src/table.ts");
    assert_eq!(
        CapabilityReferenceQuery::new(&facts, file)
            .named("map", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}

#[test]
fn framework_transforms_never_override_exact_name_buckets() {
    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        ("src/middleware/auth.ts", "export function auth() {}\n"),
        ("src/exact.ts", "export function authMiddleware() {}\n"),
        (
            "src/routes.ts",
            "export function wire() { authMiddleware(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    let target = capability_symbol(&facts, "src/exact.ts", "authMiddleware");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("authMiddleware", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        super::EXACT_PROJECT_PROVENANCE
    );

    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        (
            "src/middleware/auth.ts",
            "export function auth() {}\nexport function authMiddleware() {}\n",
        ),
        (
            "src/middleware/other.ts",
            "export function authMiddleware() {}\n",
        ),
        (
            "src/routes.ts",
            "export function wire() { authMiddleware(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    assert_eq!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("authMiddleware", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}

#[test]
fn framework_transforms_abstain_on_private_members_context_ties_and_foreign_packages() {
    let facts = generation(&[
        (
            "package.json",
            r#"{"name":"app","dependencies":{"express":"4.18.0","react":"19.0.0"}}"#,
        ),
        (
            "src/controllers/user.ts",
            "export class UserController { private secret() {} protected internal() {} }\n",
        ),
        ("src/context/a.ts", "export function Theme() {}\n"),
        ("src/context/b.ts", "export function Theme() {}\n"),
        ("packages/mail/package.json", r#"{"name":"mail"}"#),
        ("packages/mail/send.ts", "export function send() {}\n"),
        (
            "src/routes.ts",
            "export function wire() { UserController.secret(); UserController.internal(); ThemeContext(); MailService.send(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    for name in [
        "UserController.secret",
        "UserController.internal",
        "ThemeContext",
        "MailService.send",
    ] {
        assert_eq!(
            CapabilityReferenceQuery::new(&facts, owner)
                .named(name, ReferenceKind::Calls)
                .target_symbol_id,
            None,
            "{name}"
        );
    }
}

#[test]
fn middleware_case_variant_overflow_abstains() {
    let mut source = String::new();
    for mask in 0_u32..17 {
        let name = "validateBody"
            .chars()
            .enumerate()
            .map(|(bit, character)| {
                if mask & (1 << bit) == 0 {
                    character
                } else {
                    character.to_ascii_uppercase()
                }
            })
            .collect::<String>();
        source.push_str("export function ");
        source.push_str(&name);
        source.push_str("() {}\n");
    }
    let facts = generation(&[
        ("package.json", EXPRESS_MANIFEST),
        ("src/middleware/validate.ts", &source),
        (
            "src/routes.ts",
            "export function wire() { VALIDATEBODY(); }\n",
        ),
    ]);
    let owner = capability_symbol(&facts, "src/routes.ts", "wire");
    assert_eq!(
        CapabilityReferenceQuery::new(&facts, owner)
            .named("VALIDATEBODY", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}

#[test]
fn frozen_typescript_corpus_keeps_the_v1_default_public_cache_target() {
    let facts = generation(&[
        (
            "src/shared/models.ts",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/typescript/src/shared/models.ts"
            ),
        ),
        (
            "src/services/user-service.ts",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/typescript/src/services/user-service.ts"
            ),
        ),
    ]);
    let owner = capability_symbol(&facts, "src/services/user-service.ts", "UserService::save");
    let target = capability_symbol(&facts, "src/shared/models.ts", "TinyCache::remember");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("remember", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        "native-explicit-receiver-type"
    );
}

#[test]
fn frozen_javascript_corpus_keeps_the_v1_mail_helper_target_with_framework_provenance() {
    let facts = generation(&[
        (
            "package.json",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/javascript/package.json"
            ),
        ),
        (
            "routes/users.js",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/javascript/routes/users.js"
            ),
        ),
        (
            "controllers/user-mail.js",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/javascript/controllers/user-mail.js"
            ),
        ),
        (
            "middleware/auth.js",
            include_str!(
                "../../../../cartograph-extract/tests/fixtures/v1_parity/javascript/middleware/auth.js"
            ),
        ),
    ]);
    let owner = capability_symbol(&facts, "routes/users.js", "wire");
    let target = capability_symbol(&facts, "controllers/user-mail.js", "MailService::send");
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named("MailHelper.send", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        FRAMEWORK_CONVENTION_PROVENANCE
    );
}

#[test]
fn framework_detection_accepts_explicit_imports_and_jsx_without_manifest_facts() {
    let facts = generation(&[
        ("src/middleware/auth.ts", "export function auth() {}\n"),
        (
            "src/routes.ts",
            "import express from 'express';\nexport function wire() { authMiddleware(); }\n",
        ),
        ("src/context/theme.ts", "export function Theme() {}\n"),
        (
            "src/app.tsx",
            "export function App() { return <ThemeProvider />; }\n",
        ),
    ]);
    for (path, owner, name, target_path, target_name) in [
        (
            "src/routes.ts",
            "wire",
            "authMiddleware",
            "src/middleware/auth.ts",
            "auth",
        ),
        (
            "src/app.tsx",
            "App",
            "ThemeProvider",
            "src/context/theme.ts",
            "Theme",
        ),
    ] {
        let owner = capability_symbol(&facts, path, owner);
        let target = capability_symbol(&facts, target_path, target_name);
        let reference = facts
            .references()
            .iter()
            .find(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                    && reference.reference_name == name
            })
            .unwrap_or_else(|| panic!("missing {name}: {:?}", facts.references()));
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(
            reference.resolution_provenance,
            FRAMEWORK_CONVENTION_PROVENANCE
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn javascript_member_and_framework_resolution_is_worker_count_invariant() {
    let directory = super::tempdir().unwrap_or_else(|error| panic!("fixture directory: {error}"));
    for (path, source) in [
        ("package.json", EXPRESS_MANIFEST),
        (
            "src/cache.ts",
            "export class Cache { invalidate() {} map() {} }\n",
        ),
        ("src/middleware/auth.ts", "export function auth() {}\n"),
        (
            "src/routes.ts",
            "export function wire(value: unknown) { value.invalidate(); [1].map(x => x); authMiddleware(); }\n",
        ),
    ] {
        let path = directory.path().join(path);
        let parent = path.parent().unwrap_or_else(|| panic!("fixture parent"));
        super::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("fixture directory: {error}"));
        super::fs::write(path, source).unwrap_or_else(|error| panic!("fixture file: {error}"));
    }
    let serial = super::build(directory.path(), 1).await;
    for workers in [2, 4, 8, 16] {
        let parallel = super::build(directory.path(), workers).await;
        assert_eq!(
            serial.facts().digest(),
            parallel.facts().digest(),
            "{workers} workers"
        );
        assert_eq!(serial.facts().references(), parallel.facts().references());
        assert_eq!(serial.facts().edges(), parallel.facts().edges());
    }
    let facts = serial.facts();
    let owner = capability_symbol(facts, "src/routes.ts", "wire");
    let cache = capability_symbol(facts, "src/cache.ts", "Cache::invalidate");
    let reference =
        CapabilityReferenceQuery::new(facts, owner).named("invalidate", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&cache.symbol_id));
    assert_eq!(reference.resolution_provenance, DYNAMIC_DISPATCH_PROVENANCE);
    let auth = capability_symbol(facts, "src/middleware/auth.ts", "auth");
    let reference =
        CapabilityReferenceQuery::new(facts, owner).named("authMiddleware", ReferenceKind::Calls);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&auth.symbol_id));
    assert_eq!(
        reference.resolution_provenance,
        FRAMEWORK_CONVENTION_PROVENANCE
    );
    assert_eq!(
        CapabilityReferenceQuery::new(facts, owner)
            .named("map", ReferenceKind::Calls)
            .target_symbol_id,
        None
    );
}
