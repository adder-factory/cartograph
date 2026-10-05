//! `ArkTS` extraction contracts ported from the v1 `ArkTS` extractor scenarios.

mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, ImportBindingKind, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

const INDEX_PAGE: &str = r"
import router from '@ohos.router';

@Component
struct CounterView {
  @State count: number = 0;
  build() {
    Button('Tap').onClick(() => {
      this.increment();
      router.pushUrl({ url: 'pages/Next' });
    });
  }
  increment(): void { this.count += 1; }
}

class WorkerService {
  run(): void { new Worker().start(); }
}
";

#[test]
fn arkts_structs_classes_fields_methods_imports_signatures_and_calls() {
    let extracted = extract("entry/src/main/ets/pages/Index.ets", INDEX_PAGE);
    assert_eq!(extracted.language, SourceLanguage::ArkTs);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);

    let view = symbol(&extracted, SymbolKind::Struct, "CounterView");
    assert_eq!(view.qualified_name, "CounterView");
    let count = symbol(&extracted, SymbolKind::Field, "count");
    assert_eq!(count.qualified_name, "CounterView::count");
    assert!(contains(&extracted, &view.id, &count.id));
    let increment = symbol(&extracted, SymbolKind::Method, "increment");
    assert_eq!(increment.qualified_name, "CounterView::increment");
    assert_eq!(increment.signature.as_deref(), Some("(): void"));
    let service = symbol(&extracted, SymbolKind::Class, "WorkerService");
    assert_eq!(service.qualified_name, "WorkerService");
    let run = symbol(&extracted, SymbolKind::Method, "run");
    assert_eq!(run.signature.as_deref(), Some("(): void"));
    symbol(&extracted, SymbolKind::Import, "@ohos.router");
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Default
            && binding.module_specifier == "@ohos.router"
            && binding.local_name == "router"
    }));

    let build = symbol(&extracted, SymbolKind::Method, "build");
    let calls = |owner: &ExtractedSymbol| {
        let mut names = extracted
            .references
            .iter()
            .filter(|reference| {
                reference.owner.as_ref() == Some(&owner.id)
                    && reference.kind == ReferenceKind::Calls
            })
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        names
    };
    for expected in [
        "Button",
        "increment",
        "onClick",
        "pushUrl",
        "router.pushUrl",
    ] {
        assert!(
            calls(build).contains(&expected),
            "missing call {expected}: {:?}",
            calls(build)
        );
    }
    assert!(calls(run).contains(&"start"), "{:?}", calls(run));
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&run.id)
            && reference.kind == ReferenceKind::Instantiates
            && reference.name == "Worker"
    }));
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.contains('\'') && !reference.name.contains("Tap")),
        "call names must stay literal-free: {:?}",
        extracted.references
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Module
                && !symbol.qualified_name.starts_with("router::")),
        "the grammar root must not become a module: {:?}",
        extracted.symbols
    );
}

#[test]
fn arkts_decorators_are_owned_by_the_decorated_declaration() {
    let extracted = extract(
        "entry/src/main/ets/view/ItemView.ets",
        r"
@Component
export struct ItemView {
  @Prop title: string = '';
  @Link @Watch('onChange') value: number;
  private static total: number = 0;
  @Builder header() { Text(this.title) }
  plain() {}
}

@Builder
function globalBuilder(label: string) {
  Text(label)
}

@Observed
export class Model {}

class Plain {}
",
    );
    let decorators = |symbol: &ExtractedSymbol| {
        let mut names = extracted
            .references
            .iter()
            .filter(|reference| {
                reference.owner.as_ref() == Some(&symbol.id)
                    && reference.kind == ReferenceKind::Decorates
            })
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    };
    let item_view = symbol(&extracted, SymbolKind::Struct, "ItemView");
    assert!(item_view.export.exported);
    assert_eq!(decorators(item_view), ["Component"]);
    assert_eq!(
        decorators(symbol(&extracted, SymbolKind::Field, "title")),
        ["Prop"]
    );
    assert_eq!(
        decorators(symbol(&extracted, SymbolKind::Field, "value")),
        ["Link", "Watch"]
    );
    let total = symbol(&extracted, SymbolKind::Field, "total");
    assert_eq!(total.visibility, Some(Visibility::Private));
    assert!(total.execution.static_member);
    assert_eq!(decorators(total), [] as [&str; 0]);
    assert_eq!(
        decorators(symbol(&extracted, SymbolKind::Method, "header")),
        ["Builder"]
    );
    assert!(
        decorators(symbol(&extracted, SymbolKind::Method, "plain")).is_empty(),
        "a decorator of an earlier member must not leak to the next member"
    );
    assert_eq!(
        decorators(symbol(&extracted, SymbolKind::Function, "globalBuilder")),
        ["Builder"]
    );
    let model = symbol(&extracted, SymbolKind::Class, "Model");
    assert!(model.export.exported);
    assert_eq!(decorators(model), ["Observed"]);
    assert_eq!(
        decorators(symbol(&extracted, SymbolKind::Class, "Plain")),
        [] as [&str; 0]
    );
    assert!(
        extracted
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Decorates)
            .all(|reference| reference.owner.is_some()),
        "every decorator needs its decorated owner"
    );
}

#[test]
fn arkts_declarations_keep_top_level_names_exports_enum_members_and_arrow_functions() {
    let extracted = extract(
        "entry/src/main/ets/common/util.ets",
        r"
import { f, g as gg } from './m';

const x = 1;
const arrow = (v: number): number => v + 1;

export function helper(id: number): Item { return new Item(); }

enum Color { Red, Green = 2 }

class C { p: T = 0; }
",
    );
    let x = symbol(&extracted, SymbolKind::Constant, "x");
    assert_eq!(x.qualified_name, "x");
    let arrow = symbol(&extracted, SymbolKind::Function, "arrow");
    assert_eq!(arrow.signature.as_deref(), Some("(v: number): number"));
    let helper = symbol(&extracted, SymbolKind::Function, "helper");
    assert!(helper.export.exported);
    assert_eq!(helper.signature.as_deref(), Some("(id: number): Item"));
    let color = symbol(&extracted, SymbolKind::Enum, "Color");
    for member in ["Red", "Green"] {
        let enum_member = symbol(&extracted, SymbolKind::EnumMember, member);
        assert!(contains(&extracted, &color.id, &enum_member.id));
    }
    let field = symbol(&extracted, SymbolKind::Field, "p");
    assert_eq!(field.qualified_name, "C::p");
    symbol(&extracted, SymbolKind::Import, "./m");
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Named
            && binding.module_specifier == "./m"
            && binding.imported_name == "g"
            && binding.local_name == "gg"
    }));
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Module),
        "{:?}",
        extracted.symbols
    );
}

#[test]
fn arkts_extraction_is_deterministic() {
    let first = extract("entry/src/main/ets/pages/Index.ets", INDEX_PAGE);
    assert_eq!(
        first,
        extract("entry/src/main/ets/pages/Index.ets", INDEX_PAGE)
    );
    assert_eq!(
        symbol(&first, SymbolKind::Struct, "CounterView").qualified_name,
        "CounterView"
    );
}

#[test]
fn arkts_computed_member_names_never_become_symbol_names() {
    let extracted = extract(
        "entry/src/main/ets/model/Keys.ets",
        r"
class Keys {
  [makeKey('sk_live_arkts_sentinel')](): void { helper(); }
}
",
    );
    let keys = symbol(&extracted, SymbolKind::Class, "Keys");
    let rendered = format!("{:?}", extracted.symbols);
    assert!(
        !rendered.contains("sk_live_arkts_sentinel") && !rendered.contains("makeKey"),
        "computed names are expressions, not identifiers: {rendered}"
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&keys.id)
                && reference.kind == ReferenceKind::Calls
                && reference.name == "helper"
        }),
        "the member body is still walked: {:?}",
        extracted.references
    );
}

#[test]
fn arkts_export_clauses_and_commented_decorator_stacks() {
    let extracted = extract(
        "entry/src/main/ets/common/exports.ets",
        r"
function shared(): void {}
const fallback = shared;
export { shared };
export default fallback;

@Component
struct Card {
  @Builder
  // renders the header
  header() {}
}
",
    );
    assert!(
        symbol(&extracted, SymbolKind::Function, "shared")
            .export
            .exported
    );
    let fallback = symbol(&extracted, SymbolKind::Constant, "fallback");
    assert!(fallback.export.exported && fallback.export.default_export);
    let header = symbol(&extracted, SymbolKind::Method, "header");
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&header.id)
            && reference.kind == ReferenceKind::Decorates
            && reference.name == "Builder"
    }));
}

#[test]
fn arkts_literal_keys_never_name_signatures_or_enum_constants() {
    let extracted = extract(
        "entry/src/main/ets/model/Literal.ets",
        r"
interface Keys { 'sk_live_signature'(): void; [k](): void; plain(): void; }
abstract class Base { abstract ['sk_live_abstract'](): void; }
enum Codes { 'sk_live_enum' = 1, ok = 2 }
class Dynamic { [makeKey(seed)](): void {} }
",
    );
    let rendered = format!("{:?}", extracted.symbols);
    assert!(!rendered.contains("sk_live"), "{rendered}");
    symbol(&extracted, SymbolKind::Method, "plain");
    let codes = symbol(&extracted, SymbolKind::Enum, "Codes");
    let ok = symbol(&extracted, SymbolKind::EnumMember, "ok");
    assert!(contains(&extracted, &codes.id, &ok.id));
    let dynamic = symbol(&extracted, SymbolKind::Class, "Dynamic");
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&dynamic.id)
                && reference.kind == ReferenceKind::Calls
                && reference.name == "makeKey"
        }),
        "a computed key still calls its function: {:?}",
        extracted.references
    );
}

#[test]
fn arkts_signatures_with_comments_are_not_retained() {
    let extracted = extract(
        "entry/src/main/ets/model/Commented.ets",
        r"
class Commented {
  run(/* sk_live_arkts_parameter */ value: number): void {}
  plain(value: number): void {}
}
type Alias = /* sk_live_arkts_alias */ string;
",
    );
    assert!(
        symbol(&extracted, SymbolKind::Method, "run")
            .signature
            .is_none()
    );
    assert!(
        symbol(&extracted, SymbolKind::TypeAlias, "Alias")
            .signature
            .is_none()
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "plain")
            .signature
            .as_deref(),
        Some("(value: number): void")
    );
    assert!(!format!("{:?}", extracted.symbols).contains("sk_live"));
}

#[test]
fn arkts_struct_members_do_not_inherit_the_struct_export() {
    let extracted = extract(
        "entry/src/main/ets/pages/Exported.ets",
        r"
export struct S { run() {} }
export class K { m() {} }
@Component
export struct Decorated { count: number = 0; build() {} }
",
    );
    let structure = symbol(&extracted, SymbolKind::Struct, "S");
    assert!(structure.export.exported);
    let run = symbol(&extracted, SymbolKind::Method, "run");
    assert_eq!(run.qualified_name, "S::run");
    assert!(
        !run.export.exported,
        "a struct method is not a module export"
    );
    assert!(!symbol(&extracted, SymbolKind::Method, "m").export.exported);
    let decorated = symbol(&extracted, SymbolKind::Struct, "Decorated");
    assert!(decorated.export.exported);
    assert!(
        !symbol(&extracted, SymbolKind::Method, "build")
            .export
            .exported
    );
    assert!(
        !symbol(&extracted, SymbolKind::Field, "count")
            .export
            .exported
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
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

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| {
            let available = extracted
                .symbols
                .iter()
                .map(|symbol| format!("{:?} {}", symbol.kind, symbol.qualified_name))
                .collect::<Vec<_>>();
            panic!("missing {kind:?} {name}; extracted: {available:?}")
        })
}
