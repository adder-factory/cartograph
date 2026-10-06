//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::fmt::Write;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    DYNAMIC_DISPATCH_RESOLUTION_PREFIX, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn jvm_module_names_ignore_returns_owned_by_nested_classes() {
    for (path, source) in [
        (
            "android/NestedName.java",
            "class OuterModule {\n public String getName() {\n class Helper { String label() { return \"Wrong\"; } }\n return \"OuterAlias\";\n }\n @ReactMethod public void run() {}\n}",
        ),
        (
            "android/NestedName.kt",
            "class OuterModule {\n fun getName(): String {\n class Helper { fun label(): String { return \"Wrong\" } }\n return \"OuterAlias\"\n }\n @ReactMethod fun run() {}\n}",
        ),
    ] {
        let extracted = extract(path, source);
        assert_native_symbol(&extracted, "OuterModule");
        assert_native_symbol(&extracted, "Helper");
        assert_bridge_line(&extracted, "react-native-module::OuterAlias", 4);
        assert_bridge_line(&extracted, "react-native-method::OuterAlias::run", 6);
        assert_eq!(bridge_count(&extracted, "::react-native-module::"), 1);
        assert_eq!(bridge_count(&extracted, "::react-native-method::"), 1);
    }
}

#[test]
fn jvm_module_name_literals_ignore_comment_extras() {
    for (path, source) in [
        (
            "android/CommentName.java",
            "class FeatureModule { public String getName() { return /* name */ \"FeatureAlias\"; } @ReactMethod public void run() {} }",
        ),
        (
            "android/CommentName.kt",
            "class FeatureModule { fun getName() = /* name */ \"FeatureAlias\"; @ReactMethod fun run() {} }",
        ),
        (
            "android/CommentReturn.kt",
            "class FeatureModule { fun getName(): String { return /* name */ \"FeatureAlias\" }; @ReactMethod fun run() {} }",
        ),
    ] {
        let extracted = extract(path, source);
        assert_bridge_line(&extracted, "react-native-module::FeatureAlias", 1);
        assert_bridge_line(&extracted, "react-native-method::FeatureAlias::run", 1);
        assert_eq!(bridge_count(&extracted, "::react-native-method::"), 1);
    }
}

#[test]
fn oversized_objc_export_identifiers_abstain_without_losing_bounded_exports() {
    const IDENTIFIER_BYTES: usize = 5_000;
    let source = format!(
        "@implementation RCTFeatureViewManager\nRCT_EXPORT_MODULE(Feature)\n\
         RCT_EXPORT_VIEW_PROPERTY(real, NSString)\n\
         RCT_EXPORT_VIEW_PROPERTY({}, NSString)\n\
         RCT_EXPORT_METHOD(run) {{}}\nRCT_EXPORT_METHOD({}) {{}}\n@end\n",
        "p".repeat(IDENTIFIER_BYTES),
        "m".repeat(IDENTIFIER_BYTES)
    );
    let extracted = extract("ios/OversizedExports.m", &source);
    assert_native_symbol(&extracted, "RCTFeatureViewManager");
    assert_bridge_line(&extracted, "native-view-manager::Feature", 1);
    assert_bridge_line(&extracted, "native-view-prop::Feature::real", 3);
    assert_bridge_line(&extracted, "react-native-method::Feature::run", 5);
    assert_eq!(bridge_count(&extracted, "::native-view-prop::Feature::"), 1);
    assert_eq!(
        bridge_count(&extracted, "::react-native-method::Feature::"),
        1
    );
}

#[test]
fn oversized_objc_registration_identifiers_never_become_partial_or_default_names() {
    const IDENTIFIER_BYTES: usize = 5_000;
    let long_name = "X".repeat(IDENTIFIER_BYTES);
    for declaration in [
        format!("@implementation RCTFeature\nRCT_EXPORT_MODULE({long_name})"),
        format!("@interface RCT_EXTERN_REMAP_MODULE({long_name}, RCTFeature, NSObject)"),
        format!("@implementation RCT{long_name}\nRCT_EXPORT_MODULE(Feature)"),
        format!("@interface RCT_EXTERN_MODULE(RCT{long_name}, NSObject)"),
    ] {
        let source = format!(
            "{declaration}\nRCT_EXTERN_METHOD(wrong)\n@end\n\
             @implementation RCTOther\nRCT_EXPORT_MODULE(Other)\nRCT_EXPORT_METHOD(real) {{}}\n@end"
        );
        let extracted = extract("ios/OversizedRegistration.m", &source);
        assert_native_symbol(&extracted, "RCTOther");
        assert_bridge_landmark(&extracted, "Other", "react-native-module");
        assert_bridge_landmark(&extracted, "real", "react-native-method::Other");
        assert_eq!(bridge_count(&extracted, "::react-native-module::"), 1);
        assert_eq!(bridge_count(&extracted, "::react-native-method::"), 1);
    }
}

#[test]
fn qualified_expo_names_use_the_builder_module_identity() {
    let extracted = extract(
        "android/QualifiedName.kt",
        "class FeatureModule : Module() {\n override fun definition() = ModuleDefinition {\n this.Name(\"Feature\")\n Function(\"run\") { 1 }\n } }",
    );
    assert_bridge_line(&extracted, "expo-module::Feature", 3);
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 4);
    assert_eq!(bridge_count(&extracted, "::expo-module-method::"), 1);
}

#[test]
fn qualified_swift_expo_names_keep_the_explicit_module_identity() {
    let extracted = extract(
        "ios/QualifiedName.swift",
        "class FeatureModule: Module {\n func definition() -> ModuleDefinition { ExpoModulesCore.Name(\"Feature\"); Function(\"run\") {} } }",
    );
    assert_bridge_line(&extracted, "expo-module::Feature", 2);
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 2);
    assert_eq!(bridge_count(&extracted, "::expo-module-method::"), 1);
}

#[test]
fn dynamic_qualified_expo_names_veto_guessed_exports() {
    let extracted = extract(
        "android/DynamicName.kt",
        "class FeatureModule : Module() { val moduleName = \"Feature\"\n override fun definition() = ModuleDefinition { this.Name(moduleName); Function(\"run\") { 1 } } }",
    );
    assert_native_symbol(&extracted, "FeatureModule");
    assert_native_symbol(&extracted, "definition");
    assert_eq!(bridge_count(&extracted, "::expo-module"), 0);
}

#[test]
fn oversized_qualified_expo_names_veto_guessed_exports() {
    const PADDING_BYTES: usize = 8_192;
    let source = format!(
        "class FeatureModule : Module() {{ override fun definition() = ModuleDefinition {{ this.Name(\"Feature\"{}); Function(\"run\") {{ 1 }} }} }}",
        " ".repeat(PADDING_BYTES)
    );
    let extracted = extract("android/OversizedQualifiedName.kt", &source);
    assert_native_symbol(&extracted, "FeatureModule");
    assert_native_symbol(&extracted, "definition");
    assert_eq!(bridge_count(&extracted, "::expo-module"), 0);
}

#[test]
fn expo_exports_deduplicate_names_per_registered_class_and_module() {
    let single = extract(
        "android/Repeated.kt",
        "class FeatureModule : Module() {\n override fun definition() = ModuleDefinition { Name(\"Feature\"); Function(\"run\") { 1 }; Function(\"run\") { 2 } }\n private fun ModuleDefinitionBuilder.helper() { Function(\"run\") { 3 } } }",
    );
    assert_bridge_line(&single, "expo-module-method::Feature::run", 2);
    assert_eq!(bridge_count(&single, "::expo-module-method::"), 1);
    let multiple = extract(
        "android/Separate.kt",
        "class FirstModule : Module() { override fun definition() = ModuleDefinition { Name(\"First\"); Function(\"run\") { 1 }; Function(\"run\") { 2 } } }\n class SecondModule : Module() { override fun definition() = ModuleDefinition { Name(\"Second\"); Function(\"run\") { 1 }; Function(\"run\") { 2 } } }",
    );
    assert_bridge_line(&multiple, "expo-module-method::First::run", 1);
    assert_bridge_line(&multiple, "expo-module-method::Second::run", 2);
    assert_eq!(bridge_count(&multiple, "::expo-module-method::"), 2);
    let same_identity = extract(
        "android/SameIdentity.kt",
        "class FirstModule : Module() { override fun definition() = ModuleDefinition { Name(\"Shared\"); Function(\"run\") { 1 }; Function(\"run\") { 2 } } }\n class SecondModule : Module() { override fun definition() = ModuleDefinition { Name(\"Shared\"); Function(\"run\") { 1 }; Function(\"run\") { 2 } } }",
    );
    assert_bridge_line(&same_identity, "expo-module-method::Shared::run", 1);
    assert!(same_identity.symbols.iter().any(|symbol| {
        symbol
            .qualified_name
            .contains("expo-module-method::Shared::run")
            && symbol.span.start_line() == 2
    }));
    assert_eq!(bridge_count(&same_identity, "::expo-module-method::"), 2);
}

#[test]
fn shared_bridge_work_exhaustion_rolls_back_exports_at_the_native_boundary() {
    const EVENT_COUNT: usize = 128;
    let mut source = "class FeatureModule: Module { func definition() -> ModuleDefinition { Name(\"Feature\"); Function(\"run\") {} } }\nfunc ordinary() { ".to_owned();
    source.push_str(&"sendEvent(withName: \"ready\", body: nil); ".repeat(EVENT_COUNT));
    source.push('}');
    let extracted = extract("ios/Work.swift", &source);
    assert_native_symbol(&extracted, "FeatureModule");
    assert_native_symbol(&extracted, "ordinary");
    assert_eq!(bridge_count(&extracted, "::expo-module"), 0);
    assert_eq!(bridge_count(&extracted, "react-native-event"), 0);
    assert_optional_omission(&extracted);
}

#[test]
fn qualified_swift_expo_module_bases_keep_the_registered_class_identity() {
    let extracted = extract(
        "ios/Qualified.swift",
        r#"class FeatureModule: ExpoModulesCore /* base */ . Module {
        override func definition() -> ModuleDefinition { Name("Feature"); Function("run") {} } }"#,
    );
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 2);
}

#[test]
fn qualified_expo_module_bases_keep_the_registered_class_identity() {
    let extracted = extract(
        "android/Qualified.kt",
        r#"class FeatureModule : expo.modules.kotlin.modules.Module() {
         override fun definition() = ModuleDefinition { Name("Feature"); Function("run") {} } }"#,
    );
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 2);
}

#[test]
fn unresolved_expo_string_templates_never_become_module_identities() {
    for (path, source) in [
        (
            "ios/Templates.swift",
            r#"class FeatureModule: Module { let prefix = "Feature"; func definition() -> ModuleDefinition { Name("\(prefix)"); Function("run") {} } }"#,
        ),
        (
            "android/Templates.kt",
            r#"class FeatureModule : Module() { val prefix = "Feature"; override fun definition() = ModuleDefinition { Name("$prefix"); Function("run") {} } }"#,
        ),
        (
            "ios/Identifier.swift",
            r#"class FeatureModule: Module { let prefix = "Feature"; func definition() -> ModuleDefinition { Name(`prefix`); Function("run") {} } }"#,
        ),
        (
            "android/Identifier.kt",
            r#"class FeatureModule : Module() { val prefix = "Feature"; override fun definition() = ModuleDefinition { Name(`prefix`); Function("run") {} } }"#,
        ),
    ] {
        let extracted = extract(path, source);
        assert_native_symbol(&extracted, "FeatureModule");
        assert_native_symbol(&extracted, "definition");
        assert_eq!(bridge_count(&extracted, "::expo-module"), 0);
    }
    let literal = extract(
        "android/Literal.kt",
        r#"class FeatureModule : Module() { override fun definition() = ModuleDefinition { Name("Price$"); Function("run") {} } }"#,
    );
    assert_bridge_line(&literal, "expo-module-method::Price$::run", 1);
}

#[test]
fn compact_turbo_module_specs_fit_the_shared_byte_work_budget() {
    let extracted = extract(
        "src/NativeCalculator.ts",
        "interface Spec extends TurboModule {\n  multiply(a: number, b: number): number;\n}\n\
         export default TurboModuleRegistry.getEnforcing<Spec>('Calculator');\n",
    );
    assert_bridge_line(
        &extracted,
        "turbo-module-spec-method::Calculator::multiply",
        2,
    );
    assert_eq!(extracted.diagnostics, []);
}

#[test]
fn optional_bridge_retained_output_exhaustion_preserves_native_methods() {
    const METHOD_COUNT: usize = 32;
    let mut source = "@implementation Thing\n".to_owned();
    for index in 0..METHOD_COUNT {
        writeln!(
            source,
            "- (void)perform{index}WithAForBByCInDOnEAtFFromGToHOfIAsJ:(id)value {{}}"
        )
        .unwrap_or_else(|error| panic!("native method source: {error}"));
    }
    source.push_str("@end\n");
    let extracted = extract("ios/Output.m", &source);
    assert_native_symbol(&extracted, "Thing");
    assert_eq!(symbol_count(&extracted, SymbolKind::Method), METHOD_COUNT);
    assert_eq!(bridge_count(&extracted, "::swift-objc-method::"), 0);
    assert_optional_omission(&extracted);
}

#[test]
fn optional_bridge_budget_exhaustion_preserves_native_facts_with_a_diagnostic() {
    const ALIAS_COUNT: usize = 257;
    let mut source = "export function ordinary() { return 1; }\n".to_owned();
    for index in 0..ALIAS_COUNT {
        writeln!(
            source,
            "const mod{index} = requireNativeModule('M{index}'); mod{index}.run();"
        )
        .unwrap_or_else(|error| panic!("registry aliases: {error}"));
    }
    let extracted = extract("src/Registry.ts", &source);
    assert_native_symbol(&extracted, "ordinary");
    assert_native_symbol(&extracted, "mod0");
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Resource)
    );
    assert!(!extracted.references.iter().any(|reference| {
        reference
            .resolution_name
            .as_deref()
            .is_some_and(|name| name.starts_with("M0::"))
    }));
    assert_optional_omission(&extracted);
}

#[test]
fn nested_plain_jvm_classes_do_not_exhaust_optional_bridge_work() {
    const CLASS_COUNT: usize = 40;
    let mut source = String::new();
    for index in 0..CLASS_COUNT {
        write!(source, "class C{index}{{")
            .unwrap_or_else(|error| panic!("nested class source: {error}"));
    }
    source.push_str(&"}".repeat(CLASS_COUNT));
    let extracted = extract("Plain.java", &source);
    assert_eq!(symbol_count(&extracted, SymbolKind::Class), CLASS_COUNT);
    assert_eq!(extracted.diagnostics, []);
    assert_eq!(bridge_count(&extracted, "::react-native"), 0);
}

#[test]
fn large_swift_conditional_bindings_extract_without_a_bridge_resolver() {
    const BINDING_COUNT: usize = 1_024;
    let mut source = "var value: Int? = 1\nfunc ordinary() { if ".to_owned();
    for index in 0..BINDING_COUNT {
        if index > 0 {
            source.push_str(", ");
        }
        write!(source, "let v{index} = value")
            .unwrap_or_else(|error| panic!("conditional source: {error}"));
    }
    source.push_str(" { use(v0) } }\n");
    let extracted = extract("Conditions.swift", &source);
    assert_native_symbol(&extracted, "ordinary");
    assert!(
        extracted
            .references
            .iter()
            .any(|reference| reference.name == "use")
    );
    assert_eq!(extracted.diagnostics, []);
}

#[test]
fn oversized_expo_names_never_guess_the_native_class_identity() {
    const PADDING_BYTES: usize = 8_192;
    let source = format!(
        "class FeatureModule: Module {{ func definition() -> ModuleDefinition {{\n\
         Name(\"Feature\"{})\nFunction(\"run\") {{}}\n}} }}",
        " ".repeat(PADDING_BYTES)
    );
    let extracted = extract("ios/OversizedName.swift", &source);
    assert_native_symbol(&extracted, "FeatureModule");
    assert_native_symbol(&extracted, "definition");
    assert_eq!(bridge_count(&extracted, "::expo-module"), 0);
}

#[test]
fn kotlin_builder_extension_declarations_keep_single_module_compatibility() {
    let extracted = extract(
        "android/Builder.kt",
        r#"
class FeatureModule: Module() {
  override fun definition() = ModuleDefinition { Name("Feature"); exportedFunction() }
  private fun ModuleDefinitionBuilder.exportedFunction() {
    Function("run") {}
  }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module::Feature", 3);
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 5);
}

#[test]
fn expo_external_declarations_abstain_in_files_with_multiple_modules() {
    let extracted = extract(
        "ios/External.swift",
        r#"
func shared() -> AnyDefinition { Function("wrong") {} }
class FeatureModule: Module {
  private let shared: () -> AnyDefinition = { Function("actual") {} }
  func definition() -> ModuleDefinition { Name("Feature"); shared(); composed(); Function("inside") {} }
  private func composed() -> AnyDefinition {
    _ = Function("scratch") {}
    return Function("run") {}
  }
}
class OtherModule: Module {
  func definition() -> ModuleDefinition { Name("Other"); Function("other") {} }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module-method::Feature::inside", 5);
    assert_bridge_line(&extracted, "expo-module-method::Other::other", 12);
    for name in ["wrong", "actual", "scratch", "run"] {
        assert_no_bridge_landmark(&extracted, name);
    }
}

#[test]
fn objc_cpp_consecutive_backslashes_keep_spliced_macros_commented() {
    for newline in ["\n", "\r\n"] {
        let source = format!(
            "@implementation RCTThing{newline}RCT_EXPORT_MODULE(Thing){newline}\
             // ignored \\\\{newline}RCT_EXPORT_METHOD(fake) {{}}{newline}\
             RCT_EXPORT_METHOD(real) {{}}{newline}@end{newline}"
        );
        let extracted = extract("ios/Splice.mm", &source);
        assert_bridge_line(&extracted, "react-native-module::Thing", 2);
        assert_bridge_line(&extracted, "react-native-method::Thing::real", 5);
        assert_no_bridge_landmark(&extracted, "fake");
    }
}

#[test]
fn jvm_native_class_defaults_ignore_annotation_literal_declarations() {
    let extracted = extract(
        "android/Annotated.kt",
        r#"
@Suppress("class FakeModule")
class OneModule {
  @ReactMethod fun run() {}
}
@Suppress("class FakeManager")
class RealViewManager {
  @ReactProp(name = "value") fun setValue(value: String) {}
}
"#,
    );
    assert_bridge_line(&extracted, "react-native-module::One", 3);
    assert_bridge_line(&extracted, "react-native-method::One::run", 4);
    assert_bridge_line(&extracted, "native-view-manager::Real", 7);
    assert_bridge_line(&extracted, "native-view-prop::Real::value", 8);
}

#[test]
fn expo_native_class_identity_ignores_annotation_literal_declarations() {
    let extracted = extract(
        "ios/Annotated.swift",
        r#"
@available(*, message: "class FakeModule: Module")
class RealModule: Module {
  public func definition() -> ModuleDefinition { Function("real") {} }
}
@available(*, message: ": Module")
class Unrelated {
  public func definition() -> ModuleDefinition { Function("unrelated") {} }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module::RealModule", 3);
    assert_bridge_line(&extracted, "expo-module-method::RealModule::real", 4);
    assert_no_bridge_landmark(&extracted, "unrelated");
}

#[test]
fn objc_cpp_crlf_line_splices_keep_commented_macros_masked() {
    let source = "@implementation RCTThing\r\nRCT_EXPORT_MODULE(Thing)\r\n\
                  // ignored \\\r\nRCT_EXPORT_METHOD(fake) {}\r\n\
                  RCT_EXPORT_METHOD(real) {}\r\n@end\r\n";
    let extracted = extract("ios/Splice.mm", source);
    assert_bridge_line(&extracted, "react-native-method::Thing::real", 5);
    assert_no_bridge_landmark(&extracted, "fake");
}

#[test]
fn objc_empty_extern_remaps_use_the_registered_native_class_name() {
    for declaration in [
        "RCT_EXTERN_REMAP_MODULE(, RCTFirst, NSObject)",
        "RCT_EXTERN_MODULE(RCTFirst, NSObject)",
    ] {
        let source = format!("@interface {declaration}\nRCT_EXTERN_METHOD(run)\n@end\n");
        let extracted = extract("ios/EmptyRemap.m", &source);
        assert_bridge_line(&extracted, "react-native-module::First", 1);
        assert_bridge_line(&extracted, "react-native-method::First::run", 2);
    }
}

#[test]
fn objc_categories_share_the_registered_native_class_identity() {
    let extracted = extract(
        "ios/Categories.m",
        r"
@implementation RCTThing (Before)
RCT_EXPORT_METHOD(before) {}
@end
@implementation RCTThing
RCT_EXPORT_MODULE(Thing)
@end
@implementation RCTThing (After)
RCT_REMAP_METHOD(after, nativeAfter) {}
@end
@implementation RCTUnregistered (Extras)
RCT_EXPORT_METHOD(unrelated) {}
@end
",
    );
    assert_bridge_line(&extracted, "react-native-method::Thing::before", 3);
    assert_bridge_line(&extracted, "react-native-method::Thing::after", 9);
    assert_no_bridge_landmark(&extracted, "unrelated");
}

#[test]
fn expo_swift_extensions_use_the_structural_module_owner() {
    let extracted = extract(
        "ios/Extensions.swift",
        r#"
class FeatureModule: Module {}
class Unrelated {}
extension FeatureModule {
  public func definition() -> ModuleDefinition {
    Name("Feature")
    Function("run") {}
  }
}
extension Unrelated {
  public func definition() -> ModuleDefinition { Function("unused") {} }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module::Feature", 6);
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 7);
    assert_no_bridge_landmark(&extracted, "unused");
}

#[test]
fn expo_anydefinition_declarations_keep_single_module_compatibility() {
    let extracted = extract(
        "ios/Composed.swift",
        r#"
class FeatureModule: Module {
  public func definition() -> ModuleDefinition { Name("Feature"); exportedFunction() }
  private func exportedFunction() -> AnyDefinition { Function("run") {} }
  private func unused() -> AnyDefinition { Function("unused") {} }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module::Feature", 3);
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 4);
    assert_bridge_line(&extracted, "expo-module-method::Feature::unused", 5);
}

#[test]
fn objc_cpp_raw_strings_and_numeric_comments_preserve_container_scope() {
    for literal in [
        r#"const char *text = R"tag(" @end ")tag";"#,
        "int count = 1'000; // @end\n",
        "int count = 0xA'BCD; /* @end */ char quote = '\\'';",
    ] {
        let source = format!(
            "@implementation RCTThing\nRCT_EXPORT_MODULE(Thing)\n\
             - (void)helper {{ {literal} }}\nRCT_EXPORT_METHOD(real) {{}}\n@end\n"
        );
        let extracted = extract("ios/Literals.mm", &source);
        assert_bridge_landmark(&extracted, "real", "react-native-method::Thing");
    }
}

#[test]
fn objc_cpp_literal_macros_do_not_register_or_export_bridge_members() {
    let extracted = extract(
        "ios/LiteralMacros.mm",
        r#"
@implementation RCTThing
- (void)helper {
  const char *text = R"tag(RCT_EXPORT_MODULE(Wrong) RCT_EXPORT_METHOD(fake))tag";
}
RCT_EXPORT_MODULE(Thing)
RCT_EXPORT_METHOD(real) {}
@end
"#,
    );
    assert_bridge_line(&extracted, "react-native-module::Thing", 6);
    assert_bridge_line(&extracted, "react-native-method::Thing::real", 7);
    assert_no_bridge_landmark(&extracted, "fake");
}

#[test]
fn objc_extern_macro_headers_register_distinct_native_classes() {
    let extracted = extract(
        "ios/Extern.m",
        r"
@interface RCT_EXTERN_MODULE(FirstModule, NSObject)
RCT_EXTERN_METHOD(first)
@end
@interface RCT_EXTERN_REMAP_MODULE(Second, SecondModule, NSObject)
RCT_EXTERN_METHOD(second)
@end
@interface RCT_EXTERN_MODULE(ThirdModule, NSObject)
RCT_EXTERN_METHOD(third)
@end
@implementation FirstModule (Extras)
RCT_EXPORT_METHOD(extra) {}
@end
",
    );
    assert_bridge_line(&extracted, "react-native-method::FirstModule::first", 3);
    assert_bridge_line(&extracted, "react-native-method::Second::second", 6);
    assert_bridge_line(&extracted, "react-native-method::FirstModule::extra", 12);
    assert_bridge_line(&extracted, "react-native-method::ThirdModule::third", 9);
}

#[test]
fn expo_extension_helpers_share_the_registered_module_owner() {
    let extracted = extract(
        "ios/ComposedExtension.swift",
        r#"
class FeatureModule: Module {}
extension FeatureModule {
  public func definition() -> ModuleDefinition { Name("Feature"); self.exportedFunction() }
}
extension FeatureModule {
  private func exportedFunction() -> AnyDefinition { Function("run") {} }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 7);
}

#[test]
fn many_sibling_expo_modules_keep_every_positive_identity_deterministically() {
    const MODULE_COUNT: usize = 512;
    let mut source = String::new();
    for index in 0..MODULE_COUNT {
        writeln!(
            source,
            "class Module{index} : Module() {{ override fun definition() = \
                 ModuleDefinition {{ Name(\"M{index}\"); Function(\"run\") {{}} }} }}"
        )
        .unwrap_or_else(|error| panic!("write sibling modules: {error}"));
    }
    let first = extract("android/Many.kt", &source);
    let second = extract("android/Many.kt", &source);
    assert_eq!(first, second);
    for index in 0..MODULE_COUNT {
        assert_bridge_line(
            &first,
            &format!("expo-module-method::M{index}::run"),
            u32::try_from(index + 1).unwrap_or_else(|error| panic!("line: {error}")),
        );
    }
}

#[test]
fn oversized_expo_arguments_abstain_without_losing_later_bounded_members() {
    const OVERSIZED_ARGUMENT_BYTES: usize = 8_192;
    let source = format!(
        "class FeatureModule: Module {{ func definition() -> ModuleDefinition {{\n\
         Name(\"Feature\")\nFunction(\"oversized\", {}) {{}}\nFunction(\"run\") {{}}\n}} }}",
        " ".repeat(OVERSIZED_ARGUMENT_BYTES)
    );
    let extracted = extract("ios/Bounded.swift", &source);
    assert_bridge_line(&extracted, "expo-module-method::Feature::run", 4);
    assert_no_bridge_landmark(&extracted, "oversized");
}

#[test]
fn large_bridge_sources_honor_cancellation() {
    use cartograph_extract::ExtractError;

    const CANCEL_AFTER_POLLS: usize = 512;
    const MAX_CANCELLATION_PROPAGATION_POLLS: usize = 2;
    const MODULE_COUNT: usize = 512;
    let mut source = String::new();
    for index in 0..MODULE_COUNT {
        writeln!(source, "class Module{index}: Module {{ func definition() -> ModuleDefinition {{ Name(\"M{index}\"); Function(\"run\") {{}} }} }}")
            .unwrap_or_else(|error| panic!("write cancellable modules: {error}"));
    }
    let limits = SourceLimits::new(SOURCE_LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
    let snapshot = SourceSnapshot::from_bytes("ios/Many.swift", source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot: {error}"));
    let mut extractor = NativeExtractor::new(SourceLanguage::Swift)
        .unwrap_or_else(|error| panic!("extractor: {error}"));
    let mut polls = 0;
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || {
            polls += 1;
            polls > CANCEL_AFTER_POLLS
        }),
        Err(ExtractError::Cancelled)
    );
    assert!(polls > CANCEL_AFTER_POLLS);
    assert!(polls <= CANCEL_AFTER_POLLS + MAX_CANCELLATION_PROPAGATION_POLLS);
}

#[test]
fn expo_swift_members_use_their_enclosing_definition() {
    let extracted = extract(
        "ios/Modules.swift",
        r#"
class Helper { func unrelated() { Name("Wrong"); Function("outside") {} } }
class FirstModule: Module {
  func definition() -> ModuleDefinition {
    Name("First")
    Function("shared") {}
  }
}
class SecondModule: Module {
  func definition() -> ModuleDefinition {
    Name("Second")
    AsyncFunction("shared") {}
    Property("available") { true }
  }
  func unrelated() { Function("outside") {} }
}
class BareModule: Module {
  func definition() -> ModuleDefinition { Function("bare") {} }
}
"#,
    );
    for (identity, line) in [
        ("expo-module-method::First::shared", 6),
        ("expo-module-method::Second::shared", 12),
        ("expo-module-method::Second::available", 13),
        ("expo-module-method::BareModule::bare", 18),
    ] {
        assert_bridge_line(&extracted, identity, line);
    }
    assert_no_bridge_landmark(&extracted, "outside");
    assert_eq!(bridge_count(&extracted, "::expo-module-method::"), 4);
}

#[test]
fn react_native_objc_methods_use_their_enclosing_implementation() {
    let extracted = extract(
        "ios/Modules.m",
        r"
@interface RCTFirst : NSObject
@end
@implementation RCTFirst
RCT_EXPORT_MODULE()
RCT_EXPORT_METHOD(shared) {}
RCT_REMAP_METHOD(remapped, nativeRemapped) {}
RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(sync) {}
@end
@implementation RCTSecond
RCT_EXPORT_MODULE(SecondAlias)
RCT_EXPORT_METHOD(shared) {}
RCT_REMAP_METHOD(remapped, nativeRemapped) {}
RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(sync, id, nativeSync) {}
@end
@implementation RCTUnregistered
RCT_EXPORT_METHOD(unregistered) {}
@end
",
    );
    for module in ["First", "SecondAlias"] {
        assert_bridge_landmark(&extracted, module, "react-native-module");
        for member in ["shared", "remapped", "sync"] {
            assert_bridge_landmark(
                &extracted,
                member,
                &format!("react-native-method::{module}"),
            );
        }
    }
    assert_bridge_line(&extracted, "react-native-method::First::shared", 6);
    assert_bridge_line(&extracted, "react-native-method::SecondAlias::shared", 12);
    assert_no_bridge_landmark(&extracted, "unregistered");
    assert!(extracted.symbols.iter().any(|symbol| {
        symbol.kind == SymbolKind::Method
            && symbol.qualified_name == "RCTUnregistered::unregistered"
    }));
}

#[test]
fn react_native_objc_extern_methods_use_their_enclosing_interface() {
    let extracted = extract(
        "ios/ExternModules.m",
        r"
@interface RCT_EXTERN_MODULE(First, NSObject)
RCT_EXTERN_METHOD(shared)
RCT_EXTERN__BLOCKING_SYNCHRONOUS_METHOD(sync)
@end
@interface RCT_EXTERN_REMAP_MODULE(SecondAlias, Second, NSObject)
RCT_EXTERN_METHOD(shared)
RCT_EXTERN_REMAP_METHOD(remapped, nativeRemapped)
@end
",
    );
    for (identity, line) in [
        ("react-native-method::First::shared", 3),
        ("react-native-method::First::sync", 4),
        ("react-native-method::SecondAlias::shared", 7),
        ("react-native-method::SecondAlias::remapped", 8),
    ] {
        assert_bridge_line(&extracted, identity, line);
    }
}

#[test]
fn react_native_jvm_methods_use_their_enclosing_class() {
    for (path, source) in [
        (
            "android/Modules.kt",
            r#"
@ReactModule(name = "First")
class FirstModule { @ReactMethod fun shared() {} }
@ReactModule(name = "Second")
class SecondModule { @ReactMethod fun shared() {} }
"#,
        ),
        (
            "android/Modules.java",
            r#"
@ReactModule(name = "First")
class FirstModule { @ReactMethod public void shared() {} }
@ReactModule(name = "Second")
class SecondModule { @ReactMethod public void shared() {} }
"#,
        ),
    ] {
        let extracted = extract(path, source);
        for module in ["First", "Second"] {
            assert_bridge_landmark(
                &extracted,
                "shared",
                &format!("react-native-method::{module}"),
            );
        }
    }
}

#[test]
fn expo_kotlin_members_use_their_enclosing_definition() {
    let extracted = extract(
        "android/Modules.kt",
        r#"
class FirstModule : Module() {
  override fun definition() = ModuleDefinition { Name("First"); Function("shared") {} }
}
class SecondModule : Module() {
  override fun definition() = ModuleDefinition { Name("Second"); AsyncFunction("shared") {} }
}
"#,
    );
    assert_bridge_line(&extracted, "expo-module-method::First::shared", 3);
    assert_bridge_line(&extracted, "expo-module-method::Second::shared", 6);
}

#[test]
fn native_view_properties_use_their_enclosing_manager() {
    for (path, source) in [
        (
            "ios/Views.m",
            r"
@implementation FirstViewManager
RCT_EXPORT_VIEW_PROPERTY(first, BOOL)
@end
@implementation SecondViewManager
RCT_REMAP_VIEW_PROPERTY(second, nativeSecond, BOOL)
@end
",
        ),
        (
            "android/Views.kt",
            r#"
class FirstViewManager : SimpleViewManager<View>() {
  @ReactProp(name = "first") fun setFirst(view: View, value: Boolean) {}
}
class SecondViewManager : SimpleViewManager<View>() {
  @ReactProp(name = "second") fun setSecond(view: View, value: Boolean) {}
}
"#,
        ),
        (
            "android/Views.java",
            r#"
class FirstViewManager extends SimpleViewManager<View> {
  @ReactProp(name = "first") public void setFirst(View view, boolean value) {}
}
class SecondViewManager extends SimpleViewManager<View> {
  @ReactProp(name = "second") public void setSecond(View view, boolean value) {}
}
"#,
        ),
    ] {
        let extracted = extract(path, source);
        for (property, component) in [("first", "First"), ("second", "Second")] {
            assert_bridge_landmark(&extracted, component, "native-view-manager");
            assert_bridge_landmark(
                &extracted,
                property,
                &format!("native-view-prop::{component}"),
            );
        }
    }
}

#[test]
fn nested_jvm_classes_keep_their_own_bridge_identity() {
    let extracted = extract(
        "android/Nested.kt",
        r#"
class OuterModule {
  @ReactMethod fun outer() {}
  @ReactModule(name = "InnerAlias")
  class InnerModule { @ReactMethod fun inner() {} }
}
class OuterViewManager : SimpleViewManager<View>() {
  @ReactProp(name = "outerProp") fun setOuter(view: View, value: Boolean) {}
  class InnerViewManager : SimpleViewManager<View>() {
    @ReactProp(name = "innerProp") fun setInner(view: View, value: Boolean) {}
  }
}
"#,
    );
    for (member, module) in [("outer", "Outer"), ("inner", "InnerAlias")] {
        assert_bridge_landmark(
            &extracted,
            member,
            &format!("react-native-method::{module}"),
        );
    }
    for (property, component) in [("outerProp", "Outer"), ("innerProp", "Inner")] {
        assert_bridge_landmark(
            &extracted,
            property,
            &format!("native-view-prop::{component}"),
        );
    }
    for category in ["react-native-method", "native-view-prop"] {
        assert_eq!(bridge_count(&extracted, &format!("::{category}::")), 2);
    }
}

#[test]
fn unresolved_jvm_module_names_do_not_borrow_nested_class_literals() {
    let extracted = extract(
        "android/Names.kt",
        r#"
@ReactModule(name = UNKNOWN_NAME)
class OuterModule {
  fun getName() = UNKNOWN_NAME
  @ReactMethod fun outer() {}
  @ReactModule(name = "InnerAlias")
  class InnerModule { @ReactMethod fun inner() {} }
}
"#,
    );
    assert_bridge_landmark(&extracted, "inner", "react-native-method::InnerAlias");
    assert_bridge_line(&extracted, "react-native-method::Outer::outer", 5);
}

#[test]
fn react_native_jvm_literal_names_stay_with_their_methods() {
    for (path, source) in [
        (
            "android/Names.kt",
            r#"
class FirstModule {
  fun getName(): String = "FirstAlias"
  @ReactMethod fun shared() {}
}
class SecondModule {
  fun getName(): String { return "SecondAlias" }
  @ReactMethod fun shared() {}
}
"#,
        ),
        (
            "android/Names.java",
            r#"
class FirstModule {
  public String getName() { return "FirstAlias"; }
  @ReactMethod public void shared() {}
}
class SecondModule {
  public String getName() { return "SecondAlias"; }
  @ReactMethod public void shared() {}
}
"#,
        ),
    ] {
        let extracted = extract(path, source);
        assert_bridge_line(&extracted, "react-native-method::FirstAlias::shared", 4);
        assert_bridge_line(&extracted, "react-native-method::SecondAlias::shared", 8);
    }
}

#[test]
fn expo_nested_definitions_keep_their_module_identity() {
    for (path, source) in [
        (
            "ios/Nested.swift",
            r#"
class OuterModule: Module {
  func definition() -> ModuleDefinition {
    class InnerModule: Module {
      func definition() -> ModuleDefinition { Name("Inner"); Function("inner") {} }
    }
    Name("Outer")
    Function("outer") {}
  }
}
"#,
        ),
        (
            "android/NestedExpo.kt",
            r#"
class OuterModule : Module() {
  override fun definition() = ModuleDefinition {
    class InnerModule : Module() {
      override fun definition() = ModuleDefinition { Name("Inner"); Function("inner") {} }
    }
    Name("Outer")
    Function("outer") {}
  }
}
"#,
        ),
    ] {
        let extracted = extract(path, source);
        assert_bridge_line(&extracted, "expo-module-method::Inner::inner", 5);
        assert_bridge_line(&extracted, "expo-module-method::Outer::outer", 8);
        assert_eq!(bridge_count(&extracted, "::expo-module-method::"), 2);
    }
}

#[test]
fn unresolved_expo_arguments_do_not_borrow_nested_definition_literals() {
    for (path, source) in [
        (
            "ios/Unresolved.swift",
            r#"
class OuterModule: Module {
  func definition() -> ModuleDefinition {
    Name(UNKNOWN_NAME)
    Function(UNKNOWN_METHOD) {}
    class InnerModule: Module {
      func definition() -> ModuleDefinition { Name("Inner"); Function("inner") {} }
    }
    Function("outer") {}
  }
}
"#,
        ),
        (
            "android/Unresolved.kt",
            r#"
class OuterModule : Module() {
  override fun definition() = ModuleDefinition {
    Name(UNKNOWN_NAME)
    Function(UNKNOWN_METHOD) {}
    class InnerModule : Module() {
      override fun definition() = ModuleDefinition { Name("Inner"); Function("inner") {} }
    }
    Function("outer") {}
  }
}
"#,
        ),
    ] {
        let extracted = extract(path, source);
        assert_no_bridge_landmark(&extracted, "outer");
        assert_bridge_line(&extracted, "expo-module-method::Inner::inner", 7);
        assert_eq!(bridge_count(&extracted, "::expo-module-method::"), 1);
    }
}

#[test]
fn objc_bridge_scopes_ignore_literal_markers_and_protocol_expressions() {
    let extracted = extract(
        "ios/Scopes.mm",
        r#"
@protocol Forward;
@implementation RCTFirst
RCT_EXPORT_MODULE(First)
RCT_EXPORT_METHOD(run) {
  id protocol = @protocol(Forward);
  const char *text = "@end @implementation Fake";
  int count = 1'000;
}
@end
@implementation RCTSecond
RCT_EXPORT_MODULE(Second)
RCT_EXPORT_METHOD(run) {}
@end
"#,
    );
    assert_bridge_line(&extracted, "react-native-method::First::run", 5);
    assert_bridge_line(&extracted, "react-native-method::Second::run", 13);
}

#[test]
fn react_native_objc_and_jvm_exports_become_typed_native_methods() {
    let objc = extract(
        "ios/RCTGeolocation.m",
        r"
@implementation RCTGeolocation
RCT_EXPORT_MODULE(Geolocation)
RCT_EXPORT_METHOD(getCurrentPosition:(RCTResponseSenderBlock)callback) {}
RCT_REMAP_METHOD(compute, nativeCompute:(double)value) {}
RCT_EXPORT_METHOD(addListener:(NSString *)name) {}
@end
",
    );
    assert_landmark(&objc, SymbolKind::Resource, "Geolocation");
    assert_landmark(&objc, SymbolKind::Method, "getCurrentPosition");
    assert_landmark(&objc, SymbolKind::Method, "compute");
    assert_no_bridge_landmark(&objc, "addListener");

    let kotlin = extract(
        "android/ScannerModule.kt",
        r"
class ScannerModule {
  @ReactMethod
  fun startScan() {}

  @ReactMethod
  fun removeListeners(count: Int) {}
}
",
    );
    assert_landmark(&kotlin, SymbolKind::Method, "startScan");
    assert_no_bridge_landmark(&kotlin, "removeListeners");
}

#[test]
fn expo_modules_and_fabric_views_retain_js_visible_members() {
    let swift = extract(
        "ios/HapticsModule.swift",
        r#"
import ExpoModulesCore
public class HapticsModule: Module {
  public func definition() -> ModuleDefinition {
    Name("ExpoHaptics")
    AsyncFunction("notificationAsync") { }
    Function("synchronousThing") { }
    Property("isAvailable") { true }
  }
}
"#,
    );
    assert_landmark(&swift, SymbolKind::Resource, "ExpoHaptics");
    for member in ["notificationAsync", "synchronousThing", "isAvailable"] {
        assert_landmark(&swift, SymbolKind::Method, member);
    }

    let spec = extract(
        "src/MyViewNativeComponent.ts",
        r"
import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent';
interface NativeProps {
  readonly color?: string;
  enabled: boolean;
  onTap?: () => void;
}
export default codegenNativeComponent<NativeProps>('MyView');
",
    );
    assert_landmark(&spec, SymbolKind::Component, "MyView");
    for property in ["color", "enabled", "onTap"] {
        assert_landmark(&spec, SymbolKind::Property, property);
    }

    let objc_view = extract(
        "ios/RNTFooManager.m",
        r"
@interface RNTFooManager : RCTViewManager
@end
@implementation RNTFooManager
RCT_EXPORT_VIEW_PROPERTY(color, NSString)
RCT_EXPORT_VIEW_PROPERTY(enabled, BOOL)
@end
",
    );
    assert_landmark(&objc_view, SymbolKind::Component, "RNTFoo");
    assert_landmark(&objc_view, SymbolKind::Property, "color");
    assert_landmark(&objc_view, SymbolKind::Property, "enabled");
    assert_eq!(bridge_count(&objc_view, "::native-view-manager::"), 1);

    let kotlin_view = extract(
        "android/FooViewManager.kt",
        r#"
class Helper {}
class FooViewManager : SimpleViewManager<FooView>() {
  @ReactProp(name = "color")
  fun setColor(view: FooView, color: String) {}
}
"#,
    );
    assert_landmark(&kotlin_view, SymbolKind::Component, "Foo");
    assert_landmark(&kotlin_view, SymbolKind::Property, "color");
}

#[test]
fn parenthesized_registry_argument_retains_resource_and_call_hint() {
    let extracted = extract(
        "src/native.ts",
        "const m = requireNativeModule(('Feature'));\nm.run();",
    );
    assert_landmark(&extracted, SymbolKind::Resource, "Feature");
    assert_bridge_line(&extracted, "native-module-spec::Feature", 1);
    assert!(extracted.references.iter().any(|reference| {
        reference.kind == ReferenceKind::Calls
            && reference.name == "run"
            && reference.resolution_name.as_deref() == Some("Feature::run")
            && reference.span.start_line() == 2
    }));
}

#[test]
fn parenthesized_fabric_argument_retains_component_identity() {
    let extracted = extract(
        "src/MyView.ts",
        "export default codegenNativeComponent<NativeProps>(('MyView'));",
    );
    assert_landmark(&extracted, SymbolKind::Component, "MyView");
    assert_bridge_line(&extracted, "fabric-component::MyView", 1);
}

#[test]
fn javascript_native_calls_emit_bounded_module_and_method_references() {
    let javascript = extract(
        "src/native.ts",
        r"
import { NativeModules, TurboModuleRegistry } from 'react-native';
NativeModules.Geolocation.getCurrentPosition();
NativeModules.Geolocation.addListener('ignored');
const Device = TurboModuleRegistry.getEnforcing<Spec>('DeviceInfo');
const Haptics = requireNativeModule('ExpoHaptics');
Haptics.notificationAsync();
",
    );
    for module in ["Geolocation", "DeviceInfo", "ExpoHaptics"] {
        assert!(
            javascript.references.iter().any(|reference| {
                reference.kind == ReferenceKind::References && reference.name == module
            }),
            "missing native module reference {module}: {:?}",
            javascript.references
        );
    }
    assert!(javascript.references.iter().any(|reference| {
        reference.kind == ReferenceKind::Calls && reference.name == "getCurrentPosition"
    }));
    for (name, resolution_name) in [
        ("getCurrentPosition", "Geolocation::getCurrentPosition"),
        ("notificationAsync", "ExpoHaptics::notificationAsync"),
    ] {
        assert!(
            javascript.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Calls
                    && reference.name == name
                    && reference.resolution_name.as_deref() == Some(resolution_name)
            }),
            "missing qualified native lookup {resolution_name}: {:?}",
            javascript.references
        );
    }
    assert!(javascript.references.iter().any(|reference| {
        reference.kind == ReferenceKind::Calls
            && reference.name == "addListener"
            && reference.resolution_name.as_deref()
                == Some(&format!("{DYNAMIC_DISPATCH_RESOLUTION_PREFIX}addListener"))
    }));
    assert!(javascript.references.iter().all(|reference| {
        !(reference.kind == ReferenceKind::Calls
            && reference.name == "addListener"
            && reference.resolution_name.as_deref() == Some("Geolocation::addListener"))
    }));

    let turbo = extract(
        "src/NativeDeviceInfo.ts",
        r"
interface Spec extends TurboModule {
  getConstants(): { model: string; version: string };
  ping(value: string): void;
}
export default TurboModuleRegistry.getEnforcing<Spec>('DeviceInfo');
",
    );
    assert_bridge_landmark(&turbo, "getConstants", "turbo-module-spec-method");
    assert_bridge_landmark(&turbo, "ping", "turbo-module-spec-method");
}

#[test]
fn swift_objc_exports_are_explicit_selector_aware_and_comment_safe() {
    let objc = extract(
        "ios/Player.m",
        r"
@implementation Player
- (void)playWithSong:(NSString *)song {}
- (void)setDisplayName:(NSString *)name {}
// RCT_EXPORT_METHOD(commentedOut:(NSString *)value) {}
@end
",
    );
    assert_bridge_landmark(&objc, "play", "objc-swift-method");
    assert_bridge_landmark(&objc, "playWithSong", "objc-swift-method");
    assert_bridge_landmark(&objc, "displayName", "objc-swift-method");
    assert_no_bridge_landmark(&objc, "commentedOut");

    let swift = extract(
        "ios/SwiftPlayer.swift",
        r"
@objcMembers
class SwiftPlayer {
  func play(song: String) {}
  @nonobjc func internalOnly() {}
}

class ExplicitPlayer {
  @objc(playWithTrack:)
  func perform(track: String) {}
}
",
    );
    assert_bridge_landmark(&swift, "play", "swift-objc-method");
    assert_bridge_landmark(&swift, "perform", "swift-objc-method");
    assert_bridge_landmark(&swift, "playWithTrack", "swift-objc-method");
    assert_no_bridge_landmark(&swift, "internalOnly");
}

#[test]
fn react_native_event_channels_keep_static_producers_consumers_and_handlers() {
    let javascript = extract(
        "src/events.ts",
        r"
import { NativeEventEmitter, NativeModules } from 'react-native';
function handleReady() {}
const emitter = new NativeEventEmitter(NativeModules.Device);
emitter.addListener('device.ready', handleReady);
emitter.addListener(dynamicEvent, handleDynamic);
emitter.addListener('secret-token', shouldNeverPersist);
// emitter.addListener('commented.event', commentedHandler);
",
    );
    assert!(javascript.symbols.iter().any(|symbol| {
        symbol.name == "device.ready"
            && symbol
                .qualified_name
                .contains("::react-native-event-consumer::")
    }));
    assert!(javascript.references.iter().any(|reference| {
        reference.name == "handleReady" && reference.kind == ReferenceKind::Calls
    }));
    for absent in ["secret-token", "commented.event"] {
        assert!(
            javascript
                .symbols
                .iter()
                .all(|symbol| symbol.name != absent),
            "unsafe or commented event leaked: {javascript:?}"
        );
    }

    let objc = extract(
        "ios/DeviceEmitter.m",
        r#"
@implementation DeviceEmitter
- (void)ready { [self sendEventWithName:@"device.ready" body:nil]; }
@end
"#,
    );
    let swift = extract(
        "ios/SyncEmitter.swift",
        r#"
class SyncEmitter: RCTEventEmitter {
  func publish() { sendEvent(withName: "sync.finished", body: nil) }
}
"#,
    );
    let kotlin = extract(
        "android/DeviceModule.kt",
        r#"
class DeviceModule {
  fun publish() {
    reactContext.getJSModule(DeviceEventManagerModule.RCTDeviceEventEmitter::class.java)
      .emit("device.ready", null)
  }
}
"#,
    );
    for (extracted, event) in [
        (&objc, "device.ready"),
        (&swift, "sync.finished"),
        (&kotlin, "device.ready"),
    ] {
        assert!(
            extracted.symbols.iter().any(|symbol| {
                symbol.name == event
                    && symbol
                        .qualified_name
                        .contains("::react-native-event-producer::")
            }),
            "missing native event producer {event}: {extracted:?}"
        );
    }
}

fn assert_optional_omission(extracted: &cartograph_extract::ExtractedFile) {
    assert_eq!(
        extracted
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == cartograph_extract::DiagnosticCode::OptionalFactsOmitted
            })
            .count(),
        1
    );
}

fn symbol_count(extracted: &cartograph_extract::ExtractedFile, kind: SymbolKind) -> usize {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .count()
}

fn bridge_count(extracted: &cartograph_extract::ExtractedFile, category: &str) -> usize {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.qualified_name.contains(category))
        .count()
}

fn assert_native_symbol(extracted: &cartograph_extract::ExtractedFile, name: &str) {
    assert!(extracted.symbols.iter().any(|symbol| symbol.name == name));
}

fn assert_bridge_line(extracted: &cartograph_extract::ExtractedFile, identity: &str, line: u32) {
    let qualified_name = format!("{}::{identity}", extracted.path.as_str());
    let symbol = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("missing scoped bridge {identity} in {}", extracted.path));
    assert_eq!(symbol.span.start_line(), line, "{identity}");
}

fn assert_landmark(extracted: &cartograph_extract::ExtractedFile, kind: SymbolKind, name: &str) {
    assert!(
        extracted
            .symbols
            .iter()
            .any(|symbol| symbol.kind == kind && symbol.name == name),
        "missing {kind:?} {name}: {extracted:?}"
    );
}

fn assert_no_bridge_landmark(extracted: &cartograph_extract::ExtractedFile, name: &str) {
    assert!(
        extracted.symbols.iter().all(|symbol| {
            symbol.name != name
                || (!symbol.qualified_name.contains("react-native-method")
                    && !symbol.qualified_name.contains("expo-module-method")
                    && !symbol.qualified_name.contains("objc-swift-method")
                    && !symbol.qualified_name.contains("swift-objc-method"))
        }),
        "unexpected synthetic bridge landmark {name} in {}",
        extracted.path.as_str()
    );
}

fn assert_bridge_landmark(
    extracted: &cartograph_extract::ExtractedFile,
    name: &str,
    category: &str,
) {
    assert!(
        extracted.symbols.iter().any(|symbol| {
            symbol.name == name && symbol.qualified_name.contains(&format!("::{category}::"))
        }),
        "missing synthetic bridge landmark {category} {name}: {extracted:?}"
    );
}

fn extract(path: &str, source: &str) -> cartograph_extract::ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}
