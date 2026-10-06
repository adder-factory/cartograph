//! Objective-C extraction contracts ported from the v1 `objc` extractor tests.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractedFile, ExtractedSymbol, ImportBindingKind, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

const APP_SAMPLE: &str = r#"
#import <Foundation/Foundation.h>
#import "MyClass.h"

@interface MyClass : NSObject <NSCopying>
@property (nonatomic, copy) NSString *name;
- (void)greet;
- (void)doThing:(id)x with:(id)y;
+ (instancetype)shared;
@end

@implementation MyClass

- (void)greet {
    NSLog(@"Hello");
    [self doWork];
}

- (void)doThing:(id)x with:(id)y {
    [self notify:x];
}

+ (instancetype)shared {
    return [[MyClass alloc] init];
}

@end

void helperFunction(int count) {
    MyClass *obj = [MyClass shared];
    [obj greet];
}
"#;

const RN_SAMPLE: &str = r#"@implementation RNThing
RCT_EXPORT_METHOD(doSomething:(NSString *)name resolver:(RCTPromiseResolveBlock)resolve)
{
  resolve(@"ok");
}

- (void)plainMethod:(NSInteger)x with:(NSInteger)y
{
  [self helper];
}

RCT_REMAP_METHOD(getThing, getThingWithResolver:(RCTPromiseResolveBlock)resolve)
{
  resolve(@"thing");
}

RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(syncName)
{
  return @"v";
}
@end
"#;

#[test]
fn objc_app_sample_extracts_classes_selectors_properties_functions_and_imports() {
    let extracted = extract("src/App.m", APP_SAMPLE);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);

    let classes = symbols_named(&extracted, SymbolKind::Class, "MyClass");
    assert_eq!(classes.len(), 1, "one reopened class: {extracted:?}");
    assert!(classes[0].export.exported);
    assert!(!classes[0].implementation.declaration_only);

    for selector in ["greet", "doThing:with:", "shared"] {
        let definition = method_definition(&extracted, selector);
        assert_eq!(definition.qualified_name, format!("MyClass::{selector}"));
        assert!(contains(&extracted, &classes[0].id, &definition.id));
    }
    assert!(
        method_definition(&extracted, "shared")
            .execution
            .static_member
    );
    assert!(
        !method_definition(&extracted, "greet")
            .execution
            .static_member
    );
    assert!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "doThing:with:")
            .any(|symbol| symbol.implementation.declaration_only),
        "the @interface declaration stays a declaration-only method"
    );
    for truncated in ["doThing", "with"] {
        assert!(
            symbols_named(&extracted, SymbolKind::Method, truncated)
                .iter()
                .all(|symbol| is_bridge_landmark(symbol)),
            "selector keyword {truncated} leaked as a method name"
        );
    }

    let property = symbol(&extracted, SymbolKind::Property, "name");
    assert_eq!(property.qualified_name, "MyClass::name");
    for absent in ["nonatomic", "copy", "NSString"] {
        assert!(
            extracted.symbols.iter().all(|symbol| symbol.name != absent),
            "property attribute or type {absent} became a symbol: {extracted:?}"
        );
    }

    symbol(&extracted, SymbolKind::Function, "helperFunction");
    for module in ["Foundation/Foundation.h", "MyClass.h"] {
        symbol(&extracted, SymbolKind::Import, module);
        assert!(has_reference(
            &extracted,
            ReferenceQuery::new(ReferenceKind::Imports, module, None)
        ));
    }
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::IncludeSystem
            && binding.module_specifier == "Foundation/Foundation.h"
    }));
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::IncludeQuoted && binding.module_specifier == "MyClass.h"
    }));
}

#[test]
fn objc_records_superclass_and_protocol_conformance_on_the_class() {
    let extracted = extract("src/App.m", APP_SAMPLE);
    let class = symbol(&extracted, SymbolKind::Class, "MyClass");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Extends, "NSObject", Some(class))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Implements, "NSCopying", Some(class))
    ));
    assert_eq!(
        references_of_kind(&extracted, ReferenceKind::Extends),
        ["NSObject"]
    );
    assert_eq!(
        references_of_kind(&extracted, ReferenceKind::Implements),
        ["NSCopying"]
    );
}

#[test]
fn objc_message_sends_carry_full_selectors_owned_by_the_enclosing_method() {
    let extracted = extract("src/App.m", APP_SAMPLE);
    let greet = method_definition(&extracted, "greet");
    let do_thing = method_definition(&extracted, "doThing:with:");
    let shared = method_definition(&extracted, "shared");
    let helper = symbol(&extracted, SymbolKind::Function, "helperFunction");
    for (name, owner) in [
        ("NSLog", greet),
        ("doWork", greet),
        ("notify:", do_thing),
        ("MyClass.alloc", shared),
        ("init", shared),
        ("MyClass.shared", helper),
        ("obj.greet", helper),
    ] {
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Calls, name, Some(owner))
            ),
            "missing call {name} owned by {}: {:?}",
            owner.qualified_name,
            extracted.references
        );
    }
    assert!(
        extracted.references.iter().all(|reference| {
            !reference.name.starts_with("self.") && !reference.name.starts_with("super.")
        }),
        "self/super receivers must be dropped: {:?}",
        extracted.references
    );

    let worker = extract(
        "src/Worker.m",
        r"@implementation Worker
- (void)doThing:(id)x with:(id)y { }
- (void)other:(int)n { }
- (void)run {
  [self doThing:1 with:2];
  [self other:3];
  [store setValue:a forKey:b];
  [super viewDidLoad];
}
@end
",
    );
    let run = method_definition(&worker, "run");
    method_definition(&worker, "doThing:with:");
    method_definition(&worker, "other:");
    for name in [
        "doThing:with:",
        "other:",
        "store.setValue:forKey:",
        "viewDidLoad",
    ] {
        assert!(
            has_reference(
                &worker,
                ReferenceQuery::new(ReferenceKind::Calls, name, Some(run))
            ),
            "missing selector call {name}: {:?}",
            worker.references
        );
    }
    for partial in ["other", "doThing", "store.setValue", "store.setValue:"] {
        assert!(
            !has_reference(
                &worker,
                ReferenceQuery::new(ReferenceKind::Calls, partial, None)
            ),
            "partial selector {partial} leaked: {:?}",
            worker.references
        );
    }
}

#[test]
fn objc_lightweight_generics_categories_and_generic_only_classes_are_not_protocols() {
    let extracted = extract(
        "src/MyMap.m",
        r"@interface MyMap<KeyType, ObjectType> : NSObject <NSCopying, NSCoding>
- (void)setObject:(ObjectType)obj forKey:(KeyType)key;
@end
@interface Bare<T>
@end
@interface Cat : Animal
@end
@interface Cat (Extras) <Purring>
- (void)purr;
@end
",
    );
    let map = symbol(&extracted, SymbolKind::Class, "MyMap");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Extends, "NSObject", Some(map))
    ));
    for protocol in ["NSCopying", "NSCoding"] {
        assert!(has_reference(
            &extracted,
            ReferenceQuery::new(ReferenceKind::Implements, protocol, Some(map))
        ));
    }
    let method = symbol(&extracted, SymbolKind::Method, "setObject:forKey:");
    assert!(method.implementation.declaration_only);
    let cats = symbols_named(&extracted, SymbolKind::Class, "Cat");
    assert_eq!(cats.len(), 1, "a category reopens its class: {extracted:?}");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Extends, "Animal", Some(cats[0]))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Implements, "Purring", Some(cats[0]))
    ));
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "purr").qualified_name,
        "Cat::purr"
    );
    for generic in ["KeyType", "ObjectType", "T"] {
        assert!(
            !has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Implements, generic, None)
            ),
            "lightweight generic {generic} became a protocol: {:?}",
            extracted.references
        );
    }
    assert!(
        symbols_named(&extracted, SymbolKind::Class, "Extras").is_empty(),
        "a category name is not a class"
    );
}

#[test]
fn objc_protocols_typedef_bodies_and_forward_declarations_keep_their_kinds() {
    let extracted = extract(
        "src/DataSource.h",
        r"@class Forward;
@protocol Later;
@protocol DataSource <NSObject>
- (NSInteger)numberOfItems;
@property (nonatomic, readonly) NSString *title;
@end
typedef struct { int x; int y; } Point2;
typedef enum { ColorRed, ColorBlue } Color2;
typedef int Count;
",
    );
    assert_eq!(extracted.language, SourceLanguage::ObjectiveC);
    let protocol = symbol(&extracted, SymbolKind::Protocol, "DataSource");
    assert!(protocol.export.exported);
    assert!(
        symbols_named(&extracted, SymbolKind::Interface, "DataSource").is_empty(),
        "an ObjC protocol is a protocol, not an interface"
    );
    let requirement = symbol(&extracted, SymbolKind::Method, "numberOfItems");
    assert_eq!(requirement.qualified_name, "DataSource::numberOfItems");
    assert!(requirement.implementation.declaration_only);
    assert_eq!(
        symbol(&extracted, SymbolKind::Property, "title").qualified_name,
        "DataSource::title"
    );
    symbol(&extracted, SymbolKind::Struct, "Point2");
    symbol(&extracted, SymbolKind::Field, "x");
    symbol(&extracted, SymbolKind::Enum, "Color2");
    symbol(&extracted, SymbolKind::EnumMember, "ColorRed");
    symbol(&extracted, SymbolKind::TypeAlias, "Count");
    for forward in ["Forward", "Later"] {
        assert!(
            extracted
                .symbols
                .iter()
                .all(|symbol| symbol.name != forward),
            "forward declaration {forward} became a symbol: {extracted:?}"
        );
    }
}

#[test]
fn react_native_export_macros_become_native_selector_methods() {
    let extracted = extract("ios/RNThing.m", RN_SAMPLE);
    assert_eq!(
        extracted.parse_status,
        FileParseStatus::Parsed,
        "{:?}",
        extracted.diagnostics
    );
    let class = symbol(&extracted, SymbolKind::Class, "RNThing");
    for selector in [
        "doSomething:resolver:",
        "getThingWithResolver:",
        "syncName",
        "plainMethod:with:",
    ] {
        let method = method_definition(&extracted, selector);
        assert!(
            contains(&extracted, &class.id, &method.id),
            "{selector} lost its class containment"
        );
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.name != "getThing"),
        "the REMAP JS name is not the native method: {extracted:?}"
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Function),
        "a macro body call became a function: {extracted:?}"
    );
    let do_something = method_definition(&extracted, "doSomething:resolver:");
    let get_thing = method_definition(&extracted, "getThingWithResolver:");
    let plain = method_definition(&extracted, "plainMethod:with:");
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "resolve", Some(do_something))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "resolve", Some(get_thing))
    ));
    assert!(has_reference(
        &extracted,
        ReferenceQuery::new(ReferenceKind::Calls, "helper", Some(plain))
    ));
}

#[test]
fn react_native_blocking_remap_exports_its_js_name_over_the_native_selector() {
    let extracted = extract(
        "ios/RCTSettings.m",
        "@implementation RCTSettings\nRCT_EXPORT_MODULE(Settings)\nRCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(readValue, NSString *, nativeRead:(NSString *)key)\n{\n  return key;\n}\n@end\n",
    );
    let native = method_definition(&extracted, "nativeRead:");
    let landmark = symbol(&extracted, SymbolKind::Method, "readValue");
    assert_eq!(
        landmark.qualified_name,
        "ios/RCTSettings.m::react-native-method::Settings::readValue"
    );
    assert!(contains(&extracted, &native.id, &landmark.id));
}

#[test]
fn react_native_bridge_landmarks_stay_single_and_hang_off_the_native_method() {
    let extracted = extract(
        "ios/RCTGeolocation.m",
        "@implementation RCTGeolocation\nRCT_EXPORT_MODULE(Geolocation)\nRCT_EXPORT_METHOD(getCurrentPosition:(RCTResponseSenderBlock)callback) {}\nRCT_REMAP_METHOD(compute, nativeCompute:(double)value) {}\n@end\n",
    );
    let native = method_definition(&extracted, "getCurrentPosition:");
    let landmarks = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "getCurrentPosition")
        .collect::<Vec<_>>();
    assert_eq!(landmarks.len(), 1, "{landmarks:?}");
    assert_eq!(
        landmarks[0].qualified_name,
        "ios/RCTGeolocation.m::react-native-method::Geolocation::getCurrentPosition"
    );
    assert!(contains(&extracted, &native.id, &landmarks[0].id));
    method_definition(&extracted, "nativeCompute:");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !symbol.qualified_name.contains("::objc-swift-method::")),
        "export-macro methods keep only their JS bridge landmark: {extracted:?}"
    );
}

#[test]
fn react_native_macro_methods_keep_exact_original_spans_and_lines() {
    let source = "@implementation M\n// log \u{1f525} marker\nRCT_REMAP_METHOD(\n  jsName,\n  nativeName:(id)value)\n{\n  [self later];\n}\nRCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(getMap, NSDictionary<NSString *, id> *, getMapValue)\n{\n  return @{};\n}\n@end\n";
    let extracted = extract("ios/M.m", source);
    let native = method_definition(&extracted, "nativeName:");
    let macro_start = source
        .find("RCT_REMAP_METHOD")
        .unwrap_or_else(|| panic!("fixture lost its macro"));
    assert_eq!(
        native.span.start_byte(),
        u64::try_from(macro_start).unwrap_or(u64::MAX)
    );
    assert_eq!(native.span.start_line(), 3);
    assert_eq!(native.span.end_line(), 8);
    let later = extracted
        .references
        .iter()
        .find(|reference| reference.name == "later")
        .unwrap_or_else(|| panic!("missing [self later]: {:?}", extracted.references));
    assert_eq!(later.span.start_line(), 7);
    assert_eq!(later.owner.as_ref(), Some(&native.id));
    method_definition(&extracted, "getMapValue");
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !matches!(symbol.name.as_str(), "jsName" | "getMap" | "NSDictionary")),
        "REMAP leading arguments leaked: {extracted:?}"
    );
}

#[test]
fn react_native_remap_separators_ignore_comments() {
    for invocation in [
        "RCT_REMAP_METHOD(jsName /* , */, nativeMethod:(id)x)",
        "RCT_REMAP_METHOD(jsName // , < [ {\n, nativeMethod:(id)x)",
        "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(jsName, NSDictionary<NSString *, id> * /* , > ] } */, nativeMethod:(id)x)",
    ] {
        let source = format!("@implementation M\n{invocation}\n{{\n  [self later];\n}}\n@end\n");
        let file = extract("ios/Comments.m", &source);
        assert_eq!(file.parse_status, FileParseStatus::Parsed, "{invocation}");
        let native = method_definition(&file, "nativeMethod:");
        let call = file
            .references
            .iter()
            .find(|reference| reference.kind == ReferenceKind::Calls && reference.name == "later")
            .unwrap_or_else(|| panic!("missing method body call: {file:?}"));
        assert_eq!(call.owner.as_ref(), Some(&native.id));
        assert_eq!(native.span.start_line(), 2);
    }
}

#[test]
fn react_native_rewrite_ignores_substrings_and_survives_unbalanced_macros() {
    let substring = extract(
        "ios/Counter.m",
        "int MY_RCT_EXPORT_METHOD_COUNT = 1;\n@implementation Counter\n- (void)tick {}\n@end\n",
    );
    symbol(
        &substring,
        SymbolKind::Variable,
        "MY_RCT_EXPORT_METHOD_COUNT",
    );
    method_definition(&substring, "tick");

    let unbalanced = extract(
        "ios/Broken.m",
        "@implementation Broken\nRCT_EXPORT_METHOD(broken:(id)value\n{\n}\n- (void)fine {}\n@end\n",
    );
    assert_eq!(unbalanced.parse_status, FileParseStatus::Partial);
    assert!(
        unbalanced
            .symbols
            .iter()
            .all(|symbol| symbol.name != "broken:"),
        "an unbalanced macro must stay unrewritten: {unbalanced:?}"
    );
}

#[test]
fn objc_heritage_separates_generic_parameters_type_arguments_and_protocols() {
    let extracted = extract(
        "ios/Heritage.m",
        r"@interface StringList : NSArray<NSString *>
@end
@interface Pair : NSObject<NSCopying>
@end
@interface Map <Key, Value> : NSObject
@end
@interface Wrapper<T> : Base<T> <Proto>
@end
@interface Derived<Element> : GenericBase<Element>
@end
@interface Constrained<T : NSObject *> : NSObject<NSObject>
@end
@implementation Late
@end
@interface Late : Base
@end
",
    );
    for (class, base) in [
        ("StringList", "NSArray"),
        ("Pair", "NSObject"),
        ("Map", "NSObject"),
        ("Wrapper", "Base"),
        ("Derived", "GenericBase"),
        ("Constrained", "NSObject"),
        ("Late", "Base"),
    ] {
        let owner = symbol(&extracted, SymbolKind::Class, class);
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Extends, base, Some(owner))
            ),
            "{class} lost superclass {base}"
        );
    }
    assert_eq!(
        references_of_kind(&extracted, ReferenceKind::Implements),
        ["NSCopying", "Proto", "NSObject"],
        "only real protocol lists are conformances; a generic constraint is not a parameter"
    );
    let late = symbols_named(&extracted, SymbolKind::Class, "Late");
    assert_eq!(
        late.len(),
        1,
        "an interface after its implementation reopens it"
    );
    assert!(!late[0].implementation.declaration_only);
}

#[test]
fn objc_categories_and_class_extensions_augment_without_defining() {
    let category = extract(
        "ios/Shape+Extras.m",
        "@interface Shape (Extras) <Drawable>\n- (void)extra;\n@end\n@implementation Shape (Extras)\n- (void)extra {}\n@end\n",
    );
    let shapes = symbols_named(&category, SymbolKind::Class, "Shape");
    assert_eq!(shapes.len(), 1);
    assert!(
        shapes[0].implementation.declaration_only,
        "a category never defines its class"
    );
    assert!(has_reference(
        &category,
        ReferenceQuery::new(ReferenceKind::Implements, "Drawable", Some(shapes[0]))
    ));
    method_definition(&category, "extra");

    let extension = extract(
        "ios/Shape.m",
        "@interface Shape () <Hidden>\n@end\n@implementation Shape\n- (void)draw {}\n@end\n",
    );
    let shape = symbol(&extension, SymbolKind::Class, "Shape");
    assert!(!shape.implementation.declaration_only);
    assert!(has_reference(
        &extension,
        ReferenceQuery::new(ReferenceKind::Implements, "Hidden", Some(shape))
    ));
}

#[test]
fn objc_message_receivers_never_carry_literals_and_objcpp_recovers_sends() {
    let extracted = extract(
        "ios/Holder.mm",
        "#include <vector>\n@implementation Holder\n- (void)fill {\n  std::vector<int> values;\n  [self store:values];\n  [@\"sk_live_objc_receiver\" length];\n  [(id)obj bar];\n  [items[0] baz];\n}\n@end\n",
    );
    assert_eq!(extracted.parse_status, FileParseStatus::Partial);
    let fill = method_definition(&extracted, "fill");
    for name in ["store:", "length", "bar", "baz"] {
        assert!(
            has_reference(
                &extracted,
                ReferenceQuery::new(ReferenceKind::Calls, name, Some(fill))
            ),
            "missing {name}: {:?}",
            extracted.references
        );
    }
    assert!(
        !format!("{:?}", extracted.references).contains("sk_live_objc_receiver"),
        "a literal receiver leaked into a reference"
    );
}

#[test]
fn objc_extraction_is_deterministic() {
    for (path, source) in [("src/App.m", APP_SAMPLE), ("ios/RNThing.m", RN_SAMPLE)] {
        assert_eq!(extract(path, source), extract(path, source), "{path}");
    }
}

/// Half-typed declarations whose name tree-sitter recovers as a zero-width
/// MISSING identifier: none may become a nameless symbol, because one empty
/// qualified name fails validation of the whole generation.
const NAMELESS_OBJC_DECLARATIONS: [&str; 5] = [
    "@implementation\n@end\n",
    "@interface\n@end\n",
    "@protocol\n@end\n",
    "@interface Named : NSObject\n@property (nonatomic) int;\n- (void);\n@end\n",
    "@implementation Named\n- (void) {}\n- (void)kept {}\n@end\nvoid after(void) {}\n",
];

#[test]
fn objc_recovered_declarations_without_names_emit_no_symbols() {
    for source in NAMELESS_OBJC_DECLARATIONS {
        let extracted = extract("ios/Broken.m", source);
        for symbol in &extracted.symbols {
            assert!(
                !symbol.name.trim().is_empty() && !symbol.qualified_name.starts_with("::"),
                "nameless symbol from {source:?}: {symbol:?}"
            );
        }
    }
    let kept = extract("ios/Broken.m", NAMELESS_OBJC_DECLARATIONS[4]);
    let named = symbol(&kept, SymbolKind::Class, "Named");
    assert!(contains(
        &kept,
        &named.id,
        &method_definition(&kept, "kept").id
    ));
    symbol(&kept, SymbolKind::Function, "after");
}

fn method_definition<'file>(extracted: &'file ExtractedFile, name: &str) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Method
                && symbol.name == name
                && !symbol.implementation.declaration_only
                && !is_bridge_landmark(symbol)
        })
        .unwrap_or_else(|| panic!("missing method definition {name}: {extracted:?}"))
}

/// Synthetic cross-language bridge landmarks carry a `-method::` category.
fn is_bridge_landmark(symbol: &ExtractedSymbol) -> bool {
    symbol.qualified_name.contains("-method::")
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind:?} {name}: {extracted:?}"))
}

fn symbols_named<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> Vec<&'file ExtractedSymbol> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind && symbol.name == name)
        .collect()
}

fn contains(
    extracted: &ExtractedFile,
    parent: &cartograph_domain::SymbolId,
    child: &cartograph_domain::SymbolId,
) -> bool {
    extracted
        .containments
        .iter()
        .any(|containment| &containment.parent == parent && &containment.child == child)
}

/// The kind, name, and optional owner a reference must have.
#[derive(Clone, Copy)]
struct ReferenceQuery<'query> {
    kind: ReferenceKind,
    name: &'query str,
    owner: Option<&'query ExtractedSymbol>,
}

impl<'query> ReferenceQuery<'query> {
    const fn new(
        kind: ReferenceKind,
        name: &'query str,
        owner: Option<&'query ExtractedSymbol>,
    ) -> Self {
        Self { kind, name, owner }
    }
}

fn has_reference(extracted: &ExtractedFile, query: ReferenceQuery<'_>) -> bool {
    extracted.references.iter().any(|reference| {
        reference.kind == query.kind
            && reference.name == query.name
            && query
                .owner
                .is_none_or(|owner| reference.owner.as_ref() == Some(&owner.id))
    })
}

fn references_of_kind(extracted: &ExtractedFile, kind: ReferenceKind) -> Vec<&str> {
    extracted
        .references
        .iter()
        .filter(|reference| reference.kind == kind)
        .map(|reference| reference.name.as_str())
        .collect()
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::ObjectiveC, "{path}");
    let mut extractor = NativeExtractor::new(SourceLanguage::ObjectiveC)
        .unwrap_or_else(|error| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}
