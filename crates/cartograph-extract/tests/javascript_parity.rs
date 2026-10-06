//! JavaScript/TypeScript extraction parity with the retired v1 extractor.
//!
//! Each scenario reproduces a v1 acceptance case (class fields, plain
//! JavaScript heritage, type-alias contract properties, def-use, decorators,
//! dynamic/CommonJS imports, constant reads, binding tables, body type
//! consumers) together with the shapes that must stay silent.

mod dependency_ownership;

use std::fmt::Write;

use cartograph_domain::{FileParseStatus, ReferenceKind, SymbolKind, Visibility};
use cartograph_extract::{
    DiagnosticCode, ExtractError, ExtractedFile, ExtractedReference, ExtractedSymbol,
    NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn class_field_parameter_patterns_reject_excessive_nesting_without_aborting() {
    const CHILD: &str = "CARTOGRAPH_JSTS_DEEP_PARAMETER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("test executable unavailable: {error}"));
        let status = std::process::Command::new(executable)
            .args([
                "--exact",
                "class_field_parameter_patterns_reject_excessive_nesting_without_aborting",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap_or_else(|error| panic!("nested-parameter child failed to start: {error}"));
        assert!(
            status.success(),
            "nested parameters aborted the process: {status}"
        );
        return;
    }
    let source = format!(
        "class C {{ m = ({}x{}) => x; }}",
        "{a:".repeat(40_000),
        "}".repeat(40_000)
    );
    for path in ["src/deep.js", "src/deep.ts"] {
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
            .unwrap_or_else(|error| panic!("nested-parameter snapshot failed: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("nested-parameter extractor failed: {error}"));
        assert_nesting_limit(
            &extractor
                .extract(&snapshot)
                .unwrap_or_else(|error| panic!("nested-parameter extraction failed: {error}")),
        );
    }
}

#[test]
fn imported_binding_tables_enforce_the_value_reference_cap() {
    let source = imported_binding_table_source(1_024, 8_193);
    let snapshot = SourceSnapshot::from_bytes("src/table.ts", source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("binding-table snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("binding-table extractor failed: {error}"));
    assert!(
        matches!(extractor.extract(&snapshot), Err(ExtractError::OutputLimit)),
        "imported table values must obey the value-reference cap"
    );
}

fn imported_binding_table_source(declarations: usize, values: usize) -> String {
    let mut source = String::from("import { handler } from './m';\n");
    for index in 0..declarations {
        writeln!(source, "function ordinary{index}() {{}}")
            .unwrap_or_else(|error| panic!("binding-table fixture formatting failed: {error}"));
    }
    source.push_str("const table = [");
    source.push_str(&"handler, ".repeat(values));
    source.push_str("];\n/*");
    source.push_str(&"padding ".repeat(16_384));
    source.push_str("*/\n");
    source
}

fn assert_nesting_limit(file: &ExtractedFile) {
    assert_eq!(file.parse_status, FileParseStatus::Partial);
    assert!(file.symbols.is_empty() && file.references.is_empty());
    assert!(
        file.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::NestingLimitExceeded)
    );
}

#[test]
fn typescript_class_fields_split_data_fields_from_arrow_field_methods() {
    let source = r"
interface Foo {}
function memo(fn: () => void) { return fn; }
function track(): void {}
export class Box {
  field: Foo;
  cache: Map<string, Foo>;
  private count = 0;
  static readonly MAX = 3;
  #secret = 1;
  handle = (e: Event) => { this.go(); };
  wrapped = memo(() => { track(); });
  onClick = async () => { track(); };
  repo = new Map<string, Foo>();
  go(): void {}
}
";
    let file = extract("src/box.ts", source);
    assert_eq!(file, extract("src/box.ts", source));

    for name in ["field", "cache", "count", "MAX", "#secret", "repo"] {
        let field = symbol(&file, SymbolKind::Field, &format!("Box::{name}"));
        assert!(!field.export.exported, "{name} must not inherit export");
    }
    for name in ["handle", "wrapped", "onClick", "go"] {
        symbol(&file, SymbolKind::Method, &format!("Box::{name}"));
    }
    assert_eq!(
        symbol(&file, SymbolKind::Field, "Box::count").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        symbol(&file, SymbolKind::Field, "Box::#secret").visibility,
        Some(Visibility::Private)
    );
    assert!(
        symbol(&file, SymbolKind::Field, "Box::MAX")
            .execution
            .static_member
    );
    assert!(
        symbol(&file, SymbolKind::Method, "Box::onClick")
            .execution
            .async_symbol
    );
    assert_eq!(
        symbol(&file, SymbolKind::Field, "Box::cache")
            .signature
            .as_deref(),
        Some("Map<string, Foo>")
    );
    assert_eq!(
        symbol(&file, SymbolKind::Field, "Box::count").signature,
        None,
        "an initializer literal must never become a field signature"
    );
    symbol(&file, SymbolKind::Parameter, "Box::handle::e");

    for owner in ["Box::field", "Box::cache"] {
        assert_reference(&file, owner, (ReferenceKind::TypeOf, "Foo"));
    }
    assert_reference(&file, "Box::handle", (ReferenceKind::Calls, "this.go"));
    assert_reference(&file, "Box::wrapped", (ReferenceKind::Calls, "track"));
    assert_reference(&file, "Box::wrapped", (ReferenceKind::Calls, "memo"));
    assert_reference(&file, "Box::onClick", (ReferenceKind::Calls, "track"));
    assert_reference(&file, "Box::repo", (ReferenceKind::Instantiates, "Map"));
    assert!(
        references_owned_by(&file, "Box")
            .all(|reference| !matches!(reference.name.as_str(), "track" | "memo" | "this.go")),
        "field bodies leaked to the class: {:?}",
        file.references
    );
}

#[test]
fn javascript_class_fields_emit_fields_and_wrapped_handler_methods() {
    let source = r"
class Widget {
  count = 0;
  static total = 1;
  onClick = (e) => { track(e); };
  onScroll = throttle((e) => { save(e); }, 100);
  handle() { run(); }
}
";
    let file = extract("src/widget.js", source);
    symbol(&file, SymbolKind::Field, "Widget::count");
    assert!(
        symbol(&file, SymbolKind::Field, "Widget::total")
            .execution
            .static_member
    );
    for name in ["onClick", "onScroll", "handle"] {
        symbol(&file, SymbolKind::Method, &format!("Widget::{name}"));
    }
    assert_reference(&file, "Widget::onClick", (ReferenceKind::Calls, "track"));
    assert_reference(&file, "Widget::onScroll", (ReferenceKind::Calls, "save"));
    assert_reference(
        &file,
        "Widget::onScroll",
        (ReferenceKind::Calls, "throttle"),
    );
    assert!(
        references_owned_by(&file, "Widget").next().is_none(),
        "member bodies leaked to the class: {:?}",
        file.references
    );
    assert!(
        file.symbols
            .iter()
            .all(|symbol| symbol.qualified_name != "Widget::count"
                || symbol.kind != SymbolKind::Method),
        "a data field must not be a callable method"
    );
}

#[test]
fn plain_javascript_heritage_emits_static_extends_targets_only() {
    let source = r"
class A extends B {}
class C extends mod.D {}
class E extends mixin(F) {}
";
    for path in ["src/heritage.js", "src/heritage.jsx"] {
        let file = extract(path, source);
        assert_reference(&file, "A", (ReferenceKind::Extends, "B"));
        assert_reference(&file, "C", (ReferenceKind::Extends, "mod.D"));
        assert!(
            references_owned_by(&file, "E")
                .all(|reference| reference.kind != ReferenceKind::Extends),
            "a computed mixin base is not a static extends target: {:?}",
            file.references
        );
    }
}

#[test]
fn typescript_type_alias_string_literal_generic_arguments_become_contract_properties() {
    let source = r"
export interface Service<Name extends string, Req, Resp> {
  name: Name;
  request: Req;
  response: Resp;
}

export type MyServiceList = [
  Service<
    'query_apply_record',
    { pageNo: number; pageSize: number },
    { success: boolean }
  >,
  Service<
    'apply_confirm',
    { code: string },
    { success: boolean }
  >
];
type Local = Service<'local_only', A, B>;
export type UnitSystem = 'metric' | 'imperial';
export type Leaky = Service<'@STRIPE_CANARY@', A, B>;
";
    let source = &source.replace(
        "@STRIPE_CANARY@",
        &stripe_canary("0123456789abcdefABCDEF0123"),
    );
    let file = extract("services/api.ts", source);
    let alias = symbol(&file, SymbolKind::TypeAlias, "MyServiceList");
    for name in ["query_apply_record", "apply_confirm"] {
        let property = symbol(
            &file,
            SymbolKind::Property,
            &format!("MyServiceList::{name}"),
        );
        assert!(property.export.exported, "{name} inherits the alias export");
        assert_eq!(property.signature.as_deref(), Some("Service"));
        assert!(file.containments.iter().any(|containment| {
            containment.parent == alias.id && containment.child == property.id
        }));
    }
    assert!(
        !symbol(&file, SymbolKind::Property, "Local::local_only")
            .export
            .exported
    );
    assert!(
        file.symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Property
                || !matches!(symbol.name.as_str(), "metric" | "imperial")),
        "a plain string-literal union is not a contract"
    );
    assert!(
        file.symbols
            .iter()
            .all(|symbol| !symbol.name.starts_with("sk_live")),
        "a credential-shaped literal must never become a symbol name"
    );
    let tsx = extract(
        "services/api.tsx",
        "export interface Service<Name extends string, Req, Resp> { name: Name }\nexport type L = Service<'tsx_contract', A, B>;\n",
    );
    symbol(&tsx, SymbolKind::Property, "L::tsx_contract");
}

#[test]
fn def_use_reports_used_simple_locals_only() {
    let file = extract(
        "src/defuse.ts",
        r"
function f() { let x = 1; console.log(x); }
function unused() { let x = 1; }
function a() { let y = 1; return y; }
function b() { let y = 1; }
function parameter(p: number) { return p; }
class C { m() { return this.x; } }
function destructured() { const { d } = source(); return d; }
function inner() { const z = 1; const g = () => z; return g; }
function twice() { const w = 1; return w + w; }
",
    );
    let def_use = def_use_sites(&file);
    assert_eq!(
        def_use,
        vec![
            ("f".to_owned(), "x".to_owned(), 2),
            ("a".to_owned(), "y".to_owned(), 4),
            ("inner".to_owned(), "g".to_owned(), 9),
            ("twice".to_owned(), "w".to_owned(), 10),
            ("twice".to_owned(), "w".to_owned(), 10),
        ],
        "{:?}",
        file.references
    );
}

#[test]
fn def_use_covers_methods_arrow_bindings_and_class_field_methods() {
    let file = extract(
        "src/defuse.js",
        r"
const run = () => { let total = 0; total += 1; return total; };
class Svc {
  handle = () => { const value = load(); return value; };
  method() { var item = 1; if (item) { use(item); } }
}
",
    );
    let def_use = def_use_sites(&file);
    assert_eq!(
        def_use,
        vec![
            ("run".to_owned(), "total".to_owned(), 2),
            ("run".to_owned(), "total".to_owned(), 2),
            ("Svc::handle".to_owned(), "value".to_owned(), 4),
            ("Svc::method".to_owned(), "item".to_owned(), 5),
            ("Svc::method".to_owned(), "item".to_owned(), 5),
        ],
        "{:?}",
        file.references
    );
}

#[test]
fn typescript_decorators_emit_decorates_without_cross_attribution() {
    let source = r"
function Foo(_arg: string) { return (cls: any) => cls; }
function A(cls: any) { return cls; }
function B(cls: any) { return cls; }
@Foo('x')
class X {}
@A
class Foo2 {}
@B
class Bar {}
class Svc {
  @Get('/x') method() { return 1; }
  plain() { return 2; }
  @Input() name: string;
  @ng.Output() changed = new Emitter();
}
@Component({ selector: 'x' })
export class Comp {}
";
    let file = extract("src/app.ts", source);
    for (owner, name) in [
        ("X", "Foo"),
        ("Foo2", "A"),
        ("Bar", "B"),
        ("Svc::method", "Get"),
        ("Svc::name", "Input"),
        ("Svc::changed", "ng.Output"),
        ("Comp", "Component"),
    ] {
        let decorators = decorators_of(&file, owner);
        assert_eq!(
            decorators,
            vec![name.to_owned()],
            "{owner}: {:?}",
            file.references
        );
    }
    for owner in ["Svc", "Svc::plain"] {
        assert!(decorators_of(&file, owner).is_empty(), "{owner}");
    }
}

#[test]
fn javascript_decorators_on_classes_and_members_emit_decorates() {
    let source = r"
@sealed
class Panel {
  @observable value = 1;
  @action.bound save() {}
}
";
    let file = extract("src/panel.js", source);
    assert_eq!(decorators_of(&file, "Panel"), vec!["sealed".to_owned()]);
    assert_eq!(
        decorators_of(&file, "Panel::value"),
        vec!["observable".to_owned()]
    );
    assert_eq!(
        decorators_of(&file, "Panel::save"),
        vec!["action.bound".to_owned()]
    );
}

#[test]
fn commonjs_and_dynamic_import_forms_record_owned_module_imports() {
    let source = r"
import { createRequire } from 'module';
const requireCjs = createRequire(import.meta.url);
function load() { require('./m'); }
function load2() { requireCjs('./n'); }
function load3() { return require(`./o`); }
function load4() { return require('./p' as string); }
async function load5() { await import('./q', { with: { type: 'json' } }); }
async function load6() { await import('./r'); }
function guard() { requireAuth('admin'); require(`./${name}`); }
require('./top');
";
    let file = extract("src/loader.ts", source);
    for (owner, module) in [
        ("load", "./m"),
        ("load2", "./n"),
        ("load3", "./o"),
        ("load4", "./p"),
        ("load5", "./q"),
        ("load6", "./r"),
    ] {
        assert_reference(&file, owner, (ReferenceKind::Imports, module));
    }
    assert!(file.references.iter().any(|reference| {
        reference.owner.is_none()
            && reference.kind == ReferenceKind::Imports
            && reference.name == "./top"
    }));
    assert!(
        file.references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Imports)
            .all(|reference| reference.name != "admin" && !reference.name.contains("${")),
        "non-require helpers and interpolated specifiers are not module imports: {:?}",
        file.references
    );
}

#[test]
fn bound_commonjs_requires_accept_templates_and_casts() {
    let file = extract(
        "src/bound.ts",
        "const a = require(`./a`);\nconst b = require('./b' as string);\nconst { c } = require('./c');\n",
    );
    for (local, module) in [("a", "./a"), ("b", "./b"), ("c", "./c")] {
        assert!(
            file.import_bindings
                .iter()
                .any(|binding| binding.local_name == local && binding.module_specifier == module),
            "missing binding {local}: {:?}",
            file.import_bindings
        );
    }
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Calls || reference.name != "require"),
        "bound requires are module bindings, not calls: {:?}",
        file.references
    );
}

#[test]
fn shadowed_require_aliases_are_not_module_imports() {
    let file = extract(
        "src/shadow.js",
        "function require(path) { return path; }\nfunction load() { require('./x'); }\n",
    );
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports),
        "{:?}",
        file.references
    );
}

#[test]
fn screaming_constant_reads_inside_bodies_emit_references() {
    let source = r"
import { IMPORTED_LIMIT } from './limits';
const MAX_RETRY = 3;
const URL = 'x';
const HTTP2 = 2;
export function run(n: number) {
  if (n > MAX_RETRY) return IMPORTED_LIMIT;
  const o = { MAX_RETRY: 1 };
  o.MAX_RETRY = MAX_RETRY;
  MAX_RETRY_X();
  const local = new FACTORY_CLASS();
  return URL + HTTP2;
}
export const ROUTES = { a: MAX_RETRY };
if (MAX_RETRY) { console.log(1); }
";
    let file = extract("src/constants.ts", source);
    let reads = constant_reads(&file);
    assert_eq!(
        reads,
        vec![
            ("run".to_owned(), "MAX_RETRY".to_owned(), 7),
            ("run".to_owned(), "IMPORTED_LIMIT".to_owned(), 7),
            ("run".to_owned(), "MAX_RETRY".to_owned(), 9),
            ("run".to_owned(), "HTTP2".to_owned(), 12),
            ("ROUTES".to_owned(), "MAX_RETRY".to_owned(), 14),
        ],
        "{:?}",
        file.references
    );
}

#[test]
fn module_binding_tables_reference_imported_values() {
    let source = r"
import { handlerA, handlerB } from './handlers';
import fallback from './fallback';
import * as extra from './extra';
const { lazyHandler } = require('./lazy');
export const ROUTES = {
  a: handlerA,
  list: [handlerB, extra],
  handlerA,
  d: fallback,
  nested: { deep: [lazyHandler] },
  x: undefined,
  wrapped: wrap({ q: handlerB }),
  callback: () => handlerA,
  global: window,
};
export function build() {
  const local = { a: handlerA };
  return local;
}
";
    let file = extract("src/routes.ts", source);
    let mut names = references_owned_by(&file, "ROUTES")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        [
            "extra",
            "fallback",
            "handlerA",
            "handlerA",
            "handlerB",
            "handlerB",
            "lazyHandler"
        ],
        "{:?}",
        file.references
    );
    assert!(
        references_owned_by(&file, "build::local")
            .all(|reference| reference.kind != ReferenceKind::References),
        "function-local option bags are not module binding tables: {:?}",
        file.references
    );
}

#[test]
fn typescript_body_type_consumers_emit_type_of() {
    let source = r"
interface Payload {}
interface Row {}
interface Foo {}
interface Shape {}
export function body(v: unknown, opts?: import('./opts').Options) {
  call<Payload>(v);
  [1].map((x: Row) => x);
  const s = v satisfies Shape;
  return v as Foo;
}
";
    let file = extract("src/body.ts", source);
    for name in ["Payload", "Row", "Foo", "Options"] {
        assert_reference(&file, "body", (ReferenceKind::TypeOf, name));
    }
    assert_reference(&file, "body::s", (ReferenceKind::TypeOf, "Shape"));
    assert!(
        file.import_bindings.iter().any(|binding| {
            binding.module_specifier == "./opts" && binding.imported_name == "Options"
        }),
        "an inline import type needs its module binding: {:?}",
        file.import_bindings
    );
    assert!(
        file.references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::TypeOf)
            .all(|reference| !matches!(reference.name.as_str(), "string" | "number" | "unknown")),
        "predefined types are not type references"
    );
}

#[test]
fn class_field_edge_shapes_keep_annotations_and_skip_unnamed_members() {
    let source = r"
interface Handler {}
interface Item {}
class Panel {
  handle: Handler = () => { run(); };
  [computed] = 1;
  0 = 2;
  'quoted-name' = 3;
  '@STRIPE_CANARY@' = 4;
}
export const Anonymous = class {
  value: Item = make();
};
function factory() { return class { inner = build(); }; }
";
    let source = &source.replace(
        "@STRIPE_CANARY@",
        &stripe_canary("0123456789abcdefABCDEF012345"),
    );
    let file = extract("src/panel.ts", source);
    symbol(&file, SymbolKind::Method, "Panel::handle");
    assert_reference(&file, "Panel::handle", (ReferenceKind::TypeOf, "Handler"));
    assert_reference(&file, "Panel::handle", (ReferenceKind::Calls, "run"));
    symbol(&file, SymbolKind::Field, "Panel::quoted-name");
    assert!(
        file.symbols.iter().all(|symbol| !matches!(
            symbol.name.as_str(),
            "[computed]" | "computed" | "0" | "value" | "inner"
        ) && !symbol.name.starts_with("sk_live")),
        "computed, numeric, credential-shaped, and anonymous-class members have no stable \
         member identity: {:?}",
        names(&file)
    );
    // An anonymous class expression is not a declared class: its members keep
    // their facts on the enclosing symbol instead of becoming its members.
    assert_reference(&file, "Anonymous", (ReferenceKind::TypeOf, "Item"));
    assert_reference(&file, "Anonymous", (ReferenceKind::Calls, "make"));
    assert_reference(&file, "factory", (ReferenceKind::Calls, "build"));
}

#[test]
fn def_use_skips_names_that_another_binding_form_may_shadow() {
    let source = r"
function caught() { let e = 0; try { run(); } catch (e) { use(e); } return e; }
function looped(items) { let item = 0; for (const item of items) { use(item); } return item; }
function destructured(pair) { let a = 0; { const [a] = pair; use(a); } return a; }
function nested() { let helper = 1; function helper() {} return helper; }
function plain() { let kept = 1; return kept; }
";
    let file = extract("src/shadow.js", source);
    assert_eq!(
        def_use_sites(&file),
        vec![("plain".to_owned(), "kept".to_owned(), 6)],
        "a possibly shadowed local must not claim the shadowing binding's uses: {:?}",
        file.references
    );
}

#[test]
fn def_use_skips_names_rebound_by_named_class_expressions() {
    let source = r"
function f() { let K = 0; const C = class K { static value = K; }; return [C, K]; }
";
    let file = extract("src/class-shadow.js", source);
    assert!(
        def_use_sites(&file).iter().all(|(_, name, _)| name != "K"),
        "{:?}",
        file.references
    );
}

#[test]
fn constant_reads_skip_names_rebound_by_callback_parameters() {
    let source = r"
const MAX_RETRY = 3;
export function run(items: number[]) {
  items.map(MAX_RETRY => MAX_RETRY);
  items.forEach(({ LIMIT_B }) => LIMIT_B);
  try { work(); } catch (ERROR_CODE) { report(ERROR_CODE); }
  return MAX_RETRY;
}
";
    let file = extract("src/callbacks.ts", source);
    assert_eq!(
        constant_reads(&file),
        vec![("run".to_owned(), "MAX_RETRY".to_owned(), 7)],
        "a read of a callback or catch binding is not a module constant read: {:?}",
        file.references
    );
}

#[test]
fn callback_bindings_shadow_value_and_constant_reads_but_defaults_do_not() {
    let source = r"
const MAX_RETRY = 3;
function saveHandler() {}
export function wire(items: number[]) {
  items.map(MAX_RETRY => consume(MAX_RETRY));
  items.map(saveHandler => consume(saveHandler));
  return consume(saveHandler);
}
export function withDefault(x = MAX_RETRY) { return MAX_RETRY; }
";
    let file = extract("src/rebound-values.ts", source);
    let reads = references_owned_by(&file, "wire")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| (reference.name.as_str(), reference.span.start_line()))
        .collect::<Vec<_>>();
    assert_eq!(
        reads,
        [("saveHandler", 7)],
        "callback parameters shadow module values: {:?}",
        file.references
    );
    assert_eq!(
        constant_reads(&file),
        vec![("withDefault".to_owned(), "MAX_RETRY".to_owned(), 9)],
        "a parameter default is not a binding of the constant it reads: {:?}",
        file.references
    );
}

#[test]
fn represented_parameters_stay_value_targets_while_expression_names_shadow() {
    let source = r"
const MAX_RETRY = 3;
function consume(item: unknown) { return item; }
export function forward(value: unknown) { return consume(value); }
export function run(items: number[]) {
  return items.map(function MAX_RETRY() { return MAX_RETRY; });
}
";
    let file = extract("src/represented.ts", source);
    let parameter = symbol(&file, SymbolKind::Parameter, "forward::value");
    assert!(
        file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::References
                && reference.name == "value"
                && reference.owner.as_ref()
                    == file
                        .symbols
                        .iter()
                        .find(|symbol| symbol.qualified_name == "forward")
                        .map(|symbol| &symbol.id)
        }) && parameter.kind == SymbolKind::Parameter,
        "a represented parameter remains a value target: {:?}",
        file.references
    );
    assert!(
        constant_reads(&file).is_empty(),
        "a named function expression rebinds its own name: {:?}",
        file.references
    );
}

#[test]
fn def_use_respects_block_scoped_declarations() {
    let source = r"
const value = 0;
function f(flag) {
  if (flag) { const value = 1; use(value); }
  return value;
}
function g() { var hoisted = 1; { hoisted = 2; } return hoisted; }
";
    let file = extract("src/blocks.js", source);
    assert_eq!(
        def_use_sites(&file),
        vec![
            ("f".to_owned(), "value".to_owned(), 4),
            ("g".to_owned(), "hoisted".to_owned(), 7),
            ("g".to_owned(), "hoisted".to_owned(), 7),
        ],
        "a read outside a let/const block is not a use of the block's local: {:?}",
        file.references
    );
}

#[test]
fn nearest_represented_parameter_keeps_its_value_reference() {
    let source = r"
function consume(item) { return item; }
export function run(items) {
  return items.map(value => { const forward = value => consume(value); return forward(7); });
}
";
    let file = extract("src/nearest.js", source);
    assert!(
        references_owned_by(&file, "run::forward")
            .any(|reference| reference.kind == ReferenceKind::References
                && reference.name == "value"),
        "the nearest binding is a represented parameter: {:?}",
        file.references
    );
}

#[test]
fn unadmitted_callback_parameters_shadow_module_values() {
    let source = r"
const value = 0;
function consume(item) { return item; }
class C { ['run'] = value => consume(value); }
const gen = function* (value) { yield consume(value); };
";
    let file = extract("src/unadmitted.js", source);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::References
                || reference.name != "value"),
        "parameters without symbols still shadow the module value: {:?}",
        file.references
    );
}

#[test]
fn create_require_module_member_writes_withdraw_aliases() {
    let source = r"
const Module = require('module');
Module.createRequire = (path) => (spec) => spec;
const load = Module.createRequire(__filename);
function useIt() { load('./fake'); }
";
    let file = extract("src/module-write.js", source);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports || reference.name != "./fake"),
        "{:?}",
        file.references
    );
}

#[test]
fn wide_parameter_lists_are_scanned_within_a_bound() {
    let parameters = (0..5_000)
        .map(|index| format!("p{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        "const MAX_RETRY = 3;\nexport function wide() {{ return (({parameters}) => MAX_RETRY)(); }}\n"
    );
    let file = extract("src/wide.js", &source);
    assert!(
        constant_reads(&file).is_empty(),
        "an exhausted scan is conservatively treated as rebound: {:?}",
        constant_reads(&file)
    );
}

#[test]
fn create_require_factories_rewritten_after_import_are_not_aliases() {
    let source = r"
let { createRequire } = require('module');
createRequire = (path) => (spec) => spec;
const load = createRequire(__filename);
function useIt() { load('./fake'); }
";
    let file = extract("src/rewritten-factory.js", source);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports || reference.name != "./fake"),
        "{:?}",
        file.references
    );
}

#[test]
fn create_require_alias_member_writes_keep_and_pattern_writes_drop_aliases() {
    let source = r"
import { createRequire } from 'module';
const kept = createRequire(import.meta.url);
let replaced = createRequire(import.meta.url);
function useAll(replacements: { replaced: (spec: string) => unknown }) {
  kept.cache = {};
  ({ replaced } = replacements);
  kept('./kept');
  replaced('./replaced');
}
";
    let file = extract("src/alias-writes.ts", source);
    assert_reference(&file, "useAll", (ReferenceKind::Imports, "./kept"));
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports
                || reference.name != "./replaced"),
        "{:?}",
        file.references
    );
}

#[test]
fn create_require_aliases_rebound_by_loops_or_writes_are_not_aliases() {
    let source = r"
import { createRequire } from 'module';
const looped = createRequire(import.meta.url);
let rewritten = createRequire(import.meta.url);
const kept = createRequire(import.meta.url);
function useAll(callbacks: Array<(spec: string) => unknown>) {
  for (const looped of callbacks) { looped('./loop'); }
  rewritten = (spec: string) => spec;
  rewritten('./write');
  kept('./kept');
}
";
    let file = extract("src/rebound.ts", source);
    assert_reference(&file, "useAll", (ReferenceKind::Imports, "./kept"));
    assert!(
        file.references.iter().all(|reference| {
            reference.kind != ReferenceKind::Imports
                || !matches!(reference.name.as_str(), "./loop" | "./write")
        }),
        "{:?}",
        file.references
    );
}

#[test]
fn constant_reads_skip_parameter_and_parenthesized_targets() {
    let source = r"
let LIMIT_A = 1;
export function update(items: number[]) {
  items.map(ITEM_KEY => 1);
  (LIMIT_A) = 2;
  return LIMIT_A;
}
";
    let file = extract("src/targets.ts", source);
    assert_eq!(
        constant_reads(&file),
        vec![("update".to_owned(), "LIMIT_A".to_owned(), 6)],
        "{:?}",
        file.references
    );
}

#[test]
fn create_require_factories_from_a_shadowed_require_are_not_aliases() {
    let source = r"
function require(path) { return { createRequire: () => (spec) => spec }; }
const { createRequire } = require('module');
const load = createRequire(import.meta.url);
function useIt() { load('./fake'); }
";
    let file = extract("src/fake-factory.js", source);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports || reference.name != "./fake"),
        "{:?}",
        file.references
    );
}

#[test]
fn stacked_decorators_skip_comments_and_reject_computed_targets() {
    let source = r"
@sealed
export abstract class Shape {}
class Svc {
  @A
  // explains B
  @B
  method() {}
  @(factory())
  dynamic() {}
  undecorated() {}
}
";
    let file = extract("src/stacked.ts", source);
    assert_eq!(decorators_of(&file, "Shape"), vec!["sealed".to_owned()]);
    assert_eq!(
        decorators_of(&file, "Svc::method"),
        vec!["A".to_owned(), "B".to_owned()]
    );
    for owner in ["Svc", "Svc::dynamic", "Svc::undecorated"] {
        assert!(
            decorators_of(&file, owner).is_empty(),
            "{owner}: {:?}",
            file.references
        );
    }
}

#[test]
fn create_require_aliases_require_the_node_module_factory() {
    let source = r"
import { createRequire as makeRequire } from 'node:module';
import * as nodeModule from 'module';
function createRequire() { return (path: string) => path; }
const load = makeRequire(import.meta.url);
const viaModule = nodeModule.createRequire(import.meta.url);
const fake = createRequire();
const shadowed = makeRequire(import.meta.url);
function useAll() {
  load('./renamed');
  viaModule('./member');
  fake('./fake');
  require();
  require('./a', './b');
  require('');
}
function scoped(shadowed: (path: string) => unknown) { shadowed('./shadow'); }
";
    let file = extract("src/aliases.ts", source);
    assert_reference(&file, "useAll", (ReferenceKind::Imports, "./renamed"));
    assert_reference(&file, "useAll", (ReferenceKind::Imports, "./member"));
    let imports = file
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Imports)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    for unexpected in ["./fake", "./a", "./b", "", "./shadow"] {
        assert!(
            !imports.contains(&unexpected),
            "{unexpected:?} is not a proven module load: {imports:?}"
        );
    }
}

#[test]
fn constant_shorthand_reads_and_def_use_ordering() {
    let source = r"
const MAX_RETRY = 3;
export function config() { return { MAX_RETRY }; }
export function order() { early(v); let v = 1; v = 2; return { v }; }
";
    let file = extract("src/order.ts", source);
    assert_eq!(
        constant_reads(&file),
        vec![("config".to_owned(), "MAX_RETRY".to_owned(), 3)],
        "{:?}",
        file.references
    );
    assert_eq!(
        def_use_sites(&file),
        vec![
            ("order".to_owned(), "v".to_owned(), 4),
            ("order".to_owned(), "v".to_owned(), 4),
        ],
        "only occurrences after the declaration are uses: {:?}",
        file.references
    );
}

#[test]
fn binding_tables_keep_ambiguous_module_names_but_not_themselves_or_globals() {
    let source = r"
function dup() {}
function dup() {}
export const TABLE = { a: dup, self: TABLE, missing: notDeclared };
";
    let file = extract("src/table.js", source);
    let names = references_owned_by(&file, "TABLE")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["dup"], "{:?}", file.references);
}

#[test]
fn class_members_never_make_bare_value_names_ambiguous() {
    // A bare identifier can only read a lexical binding, never a class
    // member, so member symbols must neither hide the module value a read
    // names nor become the target of a read themselves.
    let source = r"
const value = 7;
function handler() {}
function save() {}
class C {
  value = 0;
  handler = () => {};
  onlyMember = 1;
  save() {}
}
function f() {
  consume(value);
  register(handler);
  queue(save);
  consume(onlyMember);
}
";
    let file = extract("src/members.js", source);
    let mut names = references_owned_by(&file, "f")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["handler", "save", "value"], "{:?}", file.references);
}

#[test]
fn only_contract_shaped_generics_publish_contract_properties() {
    // A string literal names a contract member only as the first argument of
    // a generic this file declares as an object shape with a member typed by
    // its string name parameter (`interface Rpc<Name extends string> { name:
    // Name }`). Utility types select or transform keys of another type
    // (`Omit<Model, 'debug'>` has no `debug` property;
    // `ComponentPropsWithoutRef<'button'>` has no `button`), repeating such a
    // head (a polymorphic props union) is no evidence of a contract, and
    // neither is a string transformer (`EventName<'click'>` is `'onClick'`).
    let source = r"
interface Model { debug: boolean; name: string }
interface Rpc<Name extends string, Req> { name: Name; request: Req }
interface Box<T> { value: T }
export type View = Omit<Model, 'debug'>;
export type Picked = Pick<Model, 'name'>;
export type Narrowed = Exclude<Events, 'legacy'>;
export type Loud = Uppercase<'quiet'>;
export type Louder = [Uppercase<'a'>, Uppercase<'b'>];
export type Keyed = Record<'alpha', number>;
export type Composed = Omit<Record<'debug', boolean>, 'debug'>;
export type ButtonProps = React.ComponentPropsWithoutRef<'button'>;
export type LinkProps = ComponentProps<'a'>;
export type Boxed = Box<'x'>;
export type Polymorphic = React.ComponentPropsWithoutRef<'button'> | React.ComponentPropsWithoutRef<'a'>;
export type Ops = Op<'create', A> | Op<'remove', B>;
export type Ping = Rpc<'ping', Req>;
export type Rpcs = [Rpc<'list', Req>, Rpc<'list', Req>, Rpc<'get', Req>];
type EventName<T extends string> = `on${Capitalize<T>}`;
export type Click = EventName<'click'>;
interface Tagged<T extends string> { label: string }
export type Tag = Tagged<'x'>;
export class Endpoint<Name extends string> { name!: Name }
export type Health = Endpoint<'health'>;
type Shape<Name extends string> = { name: Name };
export type Circle = Shape<'circle'>;
";
    let file = extract("src/views.ts", source);
    let mut properties = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Property)
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    properties.sort_unstable();
    assert_eq!(
        properties,
        [
            "Circle::circle",
            "Health::health",
            "Ping::ping",
            "Rpcs::get",
            "Rpcs::list"
        ],
        "{:#?}",
        names(&file)
    );
}

#[test]
fn class_field_method_parameter_defaults_stay_walked_under_the_method() {
    // Parameter defaults run when the method runs; before fields had symbols
    // the generic walk recorded their calls, so the method must keep them.
    let source = r"
interface Foo {}
function build(): Foo { return {}; }
class C {
  run = (x: Foo = build(), { y = make() } = {}) => use(x, y);
}
";
    let file = extract("src/defaults.ts", source);
    for callee in ["build", "make", "use"] {
        assert_reference(&file, "C::run", (ReferenceKind::Calls, callee));
    }
    assert!(
        references_owned_by(&file, "C").next().is_none(),
        "parameter defaults leaked to the class: {:?}",
        file.references
    );
    let foo_types = references_owned_by(&file, "C::run")
        .filter(|reference| reference.kind == ReferenceKind::TypeOf && reference.name == "Foo")
        .count();
    assert_eq!(foo_types, 1, "{:?}", file.references);
}

#[test]
fn class_field_method_annotations_are_captured_once() {
    // Walking a field method's parameter defaults must not walk its
    // annotations a second time: a duplicated inline-import binding at one
    // site would make that site's resolution ambiguous.
    let source = "export class C {\n  run = (x: import('./types').Foo = make()) => use(x);\n}\n";
    let file = extract("src/once.ts", source);
    let bindings = file
        .import_bindings
        .iter()
        .filter(|binding| binding.module_specifier == "./types")
        .count();
    assert_eq!(bindings, 1, "{:?}", file.import_bindings);
    assert_reference(&file, "C::run", (ReferenceKind::Calls, "make"));
}

#[test]
fn value_reads_of_module_bindings_survive_other_scopes_namesakes() {
    // A parameter or local of another scope (here the new class-field method
    // parameter) never hides the module binding a read names. A read that a
    // nearer scope rebinds is not the module binding's, and the resolver
    // would bind it to the module namesake, so it stays unrecorded; a
    // parameter bound only in several nested scopes stays unrecorded as
    // before, while a name bound once in the file still names that binding.
    let source = r"
const config = {};
class C {
  run = config => config;
  handle(config) { return consume(config); }
}
function f() { return consume(config); }
function g(option) { return consume(option); }
function h(option) { return consume(option); }
function k(single) { return consume(single); }
";
    let file = extract("src/lexical.js", source);
    assert_reference(&file, "f", (ReferenceKind::References, "config"));
    assert_reference(&file, "k", (ReferenceKind::References, "single"));
    for (owner, name) in [("C::handle", "config"), ("g", "option"), ("h", "option")] {
        assert!(
            references_owned_by(&file, owner)
                .all(|reference| reference.kind != ReferenceKind::References
                    || reference.name != name),
            "{owner} recorded a read of {name}: {:?}",
            file.references
        );
    }
}

#[test]
fn uncontained_object_methods_keep_namesake_reads_unrecorded() {
    // An inline object-literal method has no containing symbol, so its
    // qualified name is its bare name and the resolver's exact-name lookup
    // would bind a same-named read to it before any parameter.
    let source = r"
register({ handler() {} });
function forward(handler) { consume(handler); }
";
    let file = extract("src/inline.js", source);
    assert!(
        references_owned_by(&file, "forward")
            .all(|reference| reference.kind != ReferenceKind::References
                || reference.name != "handler"),
        "a parameter read was recorded beside an uncontained namesake: {:?}",
        file.references
    );
}

#[test]
fn constant_reads_skip_names_shadowed_by_local_declarations() {
    // A local `let`/`const`/`var`, a destructured or loop-declared local, a
    // nested function or class declaration, each shadows the module constant
    // in its scope (hoisting and TDZ included: the read before the `var` or
    // `const` still names the local). The resolver binds a bare name to the
    // exact top-level declaration first, so such a read must not be recorded
    // as a module constant read. A read outside the declaring block, or in a
    // sibling function, still names the module constant.
    let source = r"
const MAX_RETRY = 10;
function lexical() { let MAX_RETRY = 2; return MAX_RETRY; }
function destructured() { const { MAX_RETRY } = options; return MAX_RETRY; }
function hoisted() { use(MAX_RETRY); if (flag) { var MAX_RETRY = 1; } }
function early() { use(MAX_RETRY); const MAX_RETRY = 2; }
function looped() { for (let MAX_RETRY = 0; MAX_RETRY < 3; MAX_RETRY++) { use(MAX_RETRY); } return MAX_RETRY; }
function blocked() { if (flag) { const MAX_RETRY = 1; use(MAX_RETRY); } return MAX_RETRY; }
function declared() { function MAX_RETRY() {} return MAX_RETRY; }
function classy() { class MAX_RETRY {} return MAX_RETRY; }
function outer() { const MAX_RETRY = 1; return () => MAX_RETRY; }
function sibling() { return MAX_RETRY; }
class Defaulted { run = (x = MAX_RETRY) => { var MAX_RETRY = 1; return x; }; }
function looped2(xs) { for (var MAX_RETRY of xs) {} return MAX_RETRY; }
class Holder { static { var MAX_RETRY = 1; use(MAX_RETRY); } }
";
    let file = extract("src/shadowed.js", source);
    let mut reads = constant_reads(&file)
        .into_iter()
        .filter(|(owner, _, _)| !owner.is_empty())
        .collect::<Vec<_>>();
    reads.sort();
    assert_eq!(
        reads,
        vec![
            ("Defaulted::run".to_owned(), "MAX_RETRY".to_owned(), 13),
            ("blocked".to_owned(), "MAX_RETRY".to_owned(), 8),
            ("looped".to_owned(), "MAX_RETRY".to_owned(), 7),
            ("sibling".to_owned(), "MAX_RETRY".to_owned(), 12),
        ],
        "a locally shadowed name is not a module constant read: {:?}",
        file.references
    );
    let typescript = r"
import { MAX_RETRY, LIMIT_A } from './limits';
export function enumerated() { enum MAX_RETRY { A } return MAX_RETRY.A; }
export namespace Scope {
  export const LIMIT_A = 1;
  export function inner() { return LIMIT_A; }
}
export function plain() { return MAX_RETRY + LIMIT_A; }
";
    let file = extract("src/shadowed.ts", typescript);
    let mut reads = constant_reads(&file);
    reads.sort();
    // A nearer enum or namespace member answers before the import does, and
    // the resolver binds the read to it (a contained enum lexically, a
    // namespace member as the top-level symbol the walker records). Outside
    // the namespace that flattened member would still capture the imported
    // name, so `plain`'s `LIMIT_A` is not recorded.
    assert_eq!(
        reads,
        vec![
            ("enumerated".to_owned(), "MAX_RETRY".to_owned(), 3),
            ("inner".to_owned(), "LIMIT_A".to_owned(), 6),
            ("plain".to_owned(), "MAX_RETRY".to_owned(), 8),
        ],
        "{:?}",
        file.references
    );
}

#[test]
fn constant_reads_skip_type_query_member_chains() {
    let source = r"
const MAX_RETRY = { value: 1 };
type Limit = typeof MAX_RETRY.value;
type Deep = typeof MAX_RETRY.value.inner;
declare function MAKE_LIMIT<T>(value: T): T;
type Made = typeof MAKE_LIMIT<string>;
export function run(): number { return MAX_RETRY.value; }
";
    let file = extract("src/limit.ts", source);
    assert_eq!(
        constant_reads(&file),
        vec![("run".to_owned(), "MAX_RETRY".to_owned(), 7)],
        "a `typeof` operand is a type position, never a value read: {:?}",
        file.references
    );
}

#[test]
fn def_use_ignores_type_syntax_and_enum_bindings() {
    let source = r"
function f() {
  const request = {};
  return null as unknown as ((request: number) => typeof request);
}
function g() {
  const x = 1;
  { enum x { A } consume(x); }
}
function h() { const y = 1; return y; }
function k() {
  const value = 1;
  function over(value: string): string;
  function over(value: number): number;
  function over(value: string | number) { return value; }
  return over;
}
";
    let file = extract("src/types-defuse.ts", source);
    assert_eq!(
        def_use_sites(&file),
        vec![("h".to_owned(), "y".to_owned(), 10)],
        "{:?}",
        file.references
    );
}

#[test]
fn def_use_does_not_cross_class_static_blocks() {
    // A class static block has its own `var` scope: its `var x` is not the
    // enclosing function's `x`, and its reads are not the function's uses.
    let source = r"
function f() {
  var x = 1;
  class C {
    static {
      var x = 2;
      consume(x);
    }
  }
  return C;
}
function g() { var y = 1; return y; }
";
    let file = extract("src/static-block.js", source);
    assert_eq!(
        def_use_sites(&file),
        vec![("g".to_owned(), "y".to_owned(), 12)],
        "{:?}",
        file.references
    );
}

#[test]
fn binding_tables_ignore_function_local_imports() {
    let source = r"
import { handler } from './handlers';
function setup() {
  const { window } = require('./ui');
  const helper = require('./helper');
  return [window, helper];
}
import('./lazy').lazyHelper;
const picked = { x: (await import('./ui.js')).document };
const selected = (await import('./ui.js')).location;
export const TABLE = { window, helper, handler, lazyHelper, document, location, selected };
";
    let file = extract("src/table.js", source);
    let mut names = references_owned_by(&file, "TABLE")
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        ["handler", "selected"],
        "a function-local require binding is not a module value: {:?}",
        file.references
    );
}

#[test]
fn create_require_aliases_withdrawn_by_class_names_and_updates() {
    let source = r"
import { createRequire } from 'node:module';
const load = createRequire(import.meta.url);
const C = class load {
  m() { return load('./fake'); }
};
";
    let file = extract("src/class-name.mjs", source);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports
                || reference.name != "./fake"),
        "a class expression named like the alias rebinds it: {:?}",
        file.references
    );
    let updated = r"
import { createRequire } from 'node:module';
let load = createRequire(import.meta.url);
load++;
function f() { return load('./updated'); }
";
    let file = extract("src/updated.mjs", updated);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports
                || reference.name != "./updated"),
        "an update expression writes the alias: {:?}",
        file.references
    );
    let typed = r"
import { createRequire } from 'node:module';
const load = createRequire(import.meta.url);
function f() { class load {} return load; }
function g() { return load('./typed'); }
";
    let file = extract("src/typed.ts", typed);
    assert!(
        file.references.iter().all(
            |reference| reference.kind != ReferenceKind::Imports || reference.name != "./typed"
        ),
        "a TypeScript class named like the alias rebinds it: {:?}",
        file.references
    );
    let deleted = r"
const Module = require('module');
const load = Module.createRequire(__filename);
delete Module.createRequire;
function f() { return load('./deleted'); }
";
    let file = extract("src/deleted.js", deleted);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::Imports
                || reference.name != "./deleted"),
        "deleting the factory member withdraws the alias: {:?}",
        file.references
    );
    let template = r"
const { createRequire } = require(`node:module`);
const load = createRequire(__filename);
function f() { return load('./templated'); }
";
    let file = extract("src/template.js", template);
    assert_reference(&file, "f", (ReferenceKind::Imports, "./templated"));
}

#[test]
fn inline_import_types_in_assertions_are_type_consumers() {
    let source = r"
export function asserted() { return {} as import('./m').Asserted; }
export function satisfied() { return {} satisfies import('./m').Satisfied; }
export function runtime() { return (import('./m') as Promise<unknown>); }
interface Bar {}
export function constant() { return [make<Bar>()] as const; }
const request = {};
export function queried() { return consume((request: number): typeof request => request); }
export function module() { return consume((): typeof request => request); }
export function typed() { return null as unknown as ((request: number) => typeof request); }
";
    let file = extract("src/assert.ts", source);
    assert_eq!(
        references_owned_by(&file, "constant")
            .filter(|reference| reference.kind == ReferenceKind::TypeOf && reference.name == "Bar")
            .count(),
        1,
        "`as const` asserts no type, so its operand's types are not recaptured: {:?}",
        file.references
    );
    for owner in ["queried", "typed"] {
        assert!(
            references_owned_by(&file, owner)
                .all(|reference| reference.kind != ReferenceKind::TypeOf
                    || reference.name != "request"),
            "{owner}: a `typeof` of a parameter is not the module constant: {:?}",
            file.references
        );
    }
    assert_reference(&file, "module", (ReferenceKind::TypeOf, "request"));
    assert_reference(&file, "asserted", (ReferenceKind::TypeOf, "Asserted"));
    assert_reference(&file, "satisfied", (ReferenceKind::TypeOf, "Satisfied"));
    for local in ["Asserted", "Satisfied"] {
        assert!(
            file.import_bindings
                .iter()
                .all(|binding| binding.local_name != local),
            "an inline import type binds only its own site: {:?}",
            file.import_bindings
        );
    }
}

#[test]
fn decorators_on_shadowed_receivers_emit_nothing() {
    let source = r"
import * as ng from './ng';
function local(ng: any) {
  @ng.Input()
  class Shadowed {}
  return Shadowed;
}
@ng.Input()
class Module {}
";
    let file = extract("src/decorated.ts", source);
    assert!(
        decorators_of(&file, "local::Shadowed").is_empty(),
        "a decorator receiver rebound by a parameter is not the namespace import: {:?}",
        file.references
    );
    assert_eq!(decorators_of(&file, "Module"), vec!["ng.Input".to_owned()]);
    let bare = r"
function Dec() { return (target: unknown) => target; }
export function f(Dec: any) {
  @Dec class Rebound {}
  return Rebound;
}
@Dec class Kept {}
";
    let file = extract("src/bare-decorated.ts", bare);
    assert!(
        decorators_of(&file, "f::Rebound").is_empty(),
        "a bare decorator rebound by a parameter is not the module function: {:?}",
        file.references
    );
    assert_eq!(decorators_of(&file, "Kept"), vec!["Dec".to_owned()]);
}

#[test]
fn heritage_bases_resolve_only_to_visible_declarations() {
    let flattened = extract(
        "src/flattened.mjs",
        "import { MAX_RETRY } from './cfg.js';\n{ class Error {} }\nclass Failure extends Error {}\n{ const MAX_LIMIT = 1; const MAX_RETRY = 0; function Shadow() {} }\nclass Other extends Shadow {}\nfunction f() { if (flag) { const LIMIT_A = 1; } return MAX_LIMIT + MAX_RETRY + LIMIT_A; }\n",
    );
    assert!(
        references_owned_by(&flattened, "Other")
            .all(|reference| reference.kind != ReferenceKind::Extends),
        "a block function is not hoisted out of its block in a module: {:?}",
        flattened.references
    );
    assert!(
        references_owned_by(&flattened, "Failure")
            .all(|reference| reference.kind != ReferenceKind::Extends),
        "a class confined to another block is not the global base: {:?}",
        flattened.references
    );
    assert!(
        constant_reads(&flattened).is_empty(),
        "a constant confined to another block is not what a read outside it names: {:?}",
        flattened.references
    );
    let competing = extract(
        "src/competing.js",
        "const MAX_RETRY = require('./cfg').MAX_RETRY;\nfunction f() { const MAX_RETRY = 1; return MAX_RETRY; }\n{ class Base {} }\nfunction g() { class Base {} class C extends Base {} return C; }\nfunction h() { const ns = require('./base'); class D extends ns.Base {} return D; }\nif (enabled) { const lib = require('./lib'); class E extends lib.Base {} }\nfunction outer() { class Base {} function inner() { { class Base {} } class F extends Base {} return F; } return inner; }\nfunction imports() { const MAX_RETRY = 1; { const { MAX_RETRY } = require('./cfg'); return MAX_RETRY; } }\n",
    );
    assert!(
        references_owned_by(&competing, "outer::inner::F")
            .all(|reference| reference.kind != ReferenceKind::Extends),
        "a nearer container's block class would capture the outer base: {:?}",
        competing.references
    );
    assert_reference(&competing, "E", (ReferenceKind::Extends, "lib.Base"));
    assert!(
        constant_reads(&competing).is_empty(),
        "a top-level require declaration, or an enclosing local, outranks the binding the read names: {:?}",
        competing.references
    );
    assert!(
        references_owned_by(&competing, "g::C")
            .all(|reference| reference.kind != ReferenceKind::Extends),
        "a flattened block class outranks the visible local base: {:?}",
        competing.references
    );
    assert_reference(&competing, "h::D", (ReferenceKind::Extends, "ns.Base"));
    let mixins = "class Base {}\nfunction mixin(Base) { class Mixed extends Base {} return Mixed; }\nclass Plain extends Base {}\n";
    for path in ["src/mixin.js", "src/mixin.ts"] {
        let file = extract(path, mixins);
        assert_reference(&file, "Plain", (ReferenceKind::Extends, "Base"));
        assert!(
            references_owned_by(&file, "mixin::Mixed")
                .all(|reference| reference.kind != ReferenceKind::Extends),
            "{path}: a base rebound by a parameter is not the module class: {:?}",
            file.references
        );
    }
    let file = extract(
        "src/receiver-bases.js",
        "class P extends this.base {}\nclass Q extends super.base {}\nclass R extends mod.Base {}\n",
    );
    assert_reference(&file, "R", (ReferenceKind::Extends, "mod.Base"));
    let typescript = extract(
        "src/receiver-bases.ts",
        "class P extends this.base {}\nclass Q extends super.base {}\nclass R extends Base {}\n",
    );
    assert_reference(&typescript, "R", (ReferenceKind::Extends, "Base"));
    for owner in ["P", "Q"] {
        assert!(
            references_owned_by(&typescript, owner)
                .all(|reference| reference.kind != ReferenceKind::Extends),
            "{owner}: a `this`/`super` member is not a declaration base: {:?}",
            typescript.references
        );
    }
    for owner in ["P", "Q"] {
        assert!(
            references_owned_by(&file, owner)
                .all(|reference| reference.kind != ReferenceKind::Extends),
            "{owner}: a `this`/`super` member is not a declaration base: {:?}",
            file.references
        );
    }
}

#[test]
fn contract_generics_respect_local_shadowing_and_nested_wrappers() {
    let source = r"
interface Service<N extends string> { name: N }
export declare class Endpoint<N extends string> { name: N }
function f() {
  type Service<N> = N;
  type Local = Service<'go'>;
  return null as unknown as Local;
}
namespace Other { export type Service<N> = N; }
namespace Scope {
  import Service = Other.Service;
  type Aliased = Service<'aliased'>;
}
function g<Service>() {
  type Parameter = Service<'parameter'>;
}
function h() {
  switch (0) {
    case 0:
      type Service<N> = N;
      type Switched = Service<'switched'>;
  }
}
export type Remote = Service<'stop'>;
export type Health = Endpoint<'health'>;
";
    let file = extract("src/contracts.ts", source);
    let mut properties = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Property)
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    properties.sort_unstable();
    assert_eq!(
        properties,
        ["Health::health", "Remote::stop"],
        "{:#?}",
        names(&file)
    );
}

#[test]
fn literal_member_names_never_carry_credentials() {
    let source = r#"
interface Service<N extends string> { name: N }
export type Api = [
  Service<"postgres://admin:hunter2@db/prod">,
  Service<"my password hunter2">,
  Service<"Bearer abc123">,
  Service<"list users">,
];
export class Config {
  "postgres://admin:hunter2@db/prod" = 1;
  "user@example.com" = 2;
  "api secret" = 3;
  "display name" = 4;
}
"#;
    let file = extract("src/literal-names.ts", source);
    let mut members = file
        .symbols
        .iter()
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Property | SymbolKind::Field))
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    members.sort_unstable();
    assert_eq!(
        members,
        ["Api::list users", "Config::display name"],
        "{:#?}",
        names(&file)
    );
}

#[test]
fn value_reads_never_name_another_functions_parameter() {
    // A parameter is visible only inside its own function; a bare read in
    // another function names something else (a global, or an import the
    // resolver would find in another file), never that parameter.
    let source = r"
class C { handler() {} }
function own(handler) { return handler; }
function use() { consume(handler); }
function inner() { function nested(token) {} return consume(token); }
function wrap(callback) { return [1].map(() => consume(callback)); }
function load() { const cfg = require('./cfg'); return () => run(cfg); }
const shared = {};
function required() { const { shared } = require('./ui'); return consume(shared); }
function blocked() { if (flag) { const once = 1; } return consume(once); }
";
    let file = extract("src/params.js", source);
    for (owner, name) in [
        ("use", "handler"),
        ("inner", "token"),
        ("required", "shared"),
        ("blocked", "once"),
    ] {
        assert!(
            references_owned_by(&file, owner)
                .all(|reference| reference.kind != ReferenceKind::References
                    || reference.name != name),
            "{owner} recorded an inaccessible parameter {name}: {:?}",
            file.references
        );
    }
    assert_reference(&file, "wrap", (ReferenceKind::References, "callback"));
    assert_reference(&file, "load", (ReferenceKind::References, "cfg"));
}

#[test]
fn generator_class_fields_are_methods() {
    let source = r"
class Feed {
  items = async function* (page) { yield load(page); };
  data = [];
}
";
    let file = extract("src/feed.js", source);
    symbol(&file, SymbolKind::Method, "Feed::items");
    symbol(&file, SymbolKind::Field, "Feed::data");
    assert_reference(&file, "Feed::items", (ReferenceKind::Calls, "load"));
}

#[test]
fn constant_reads_of_locals_without_module_namesakes_are_recorded() {
    // With no module binding of the same name, the resolver binds the read
    // to the local itself (or, for a `require`/`import()` local, to the
    // imported value), so these reads stay recorded.
    let source = r"
export async function count(node) {
  const PARAM_KINDS = new Set(['formal_parameters']);
  return PARAM_KINDS.has(node.type);
}
export async function migrate() {
  const { SCHEMA_VERSION } = await import('./migrations.js');
  const { LIMIT_B } = require('./limits');
  return SCHEMA_VERSION + LIMIT_B;
}
";
    let file = extract("src/locals.js", source);
    assert_eq!(
        constant_reads(&file),
        vec![
            ("count".to_owned(), "PARAM_KINDS".to_owned(), 4),
            ("migrate".to_owned(), "SCHEMA_VERSION".to_owned(), 9),
            ("migrate".to_owned(), "LIMIT_B".to_owned(), 9),
        ],
        "{:?}",
        file.references
    );
}

#[test]
fn module_level_block_declarations_stay_value_targets() {
    // Declarations in a block outside every function are top-level symbols;
    // a read inside that block names them even when a parameter elsewhere
    // shares the name.
    let source = r"
function handle(request) { return request; }
while (running()) {
  const request = parse();
  const response = handle(request);
  write(response);
}
";
    let file = extract("src/worker.js", source);
    assert_reference(&file, "response", (ReferenceKind::References, "request"));
    assert!(
        file.references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::References
                && reference.name == "response"
                && reference.owner.is_none()),
        "{:?}",
        file.references
    );
}

#[test]
fn type_parameters_of_callbacks_functions_and_classes_name_no_declaration() {
    let source = r"
interface Payload {}
interface Item {}
export function f() {
  return use(<Payload>(x: Payload): Payload => x);
}
export function g<Payload>() {
  return consume<Payload>();
}
export class Box<Item> {
  open() { return make<Item>(); }
}
export function h() {
  return consume<Payload>();
}
export function k() {
  return consume<(<Payload>(x: Payload) => Payload)>();
}
namespace Types { export interface Payload {} }
export function q<Payload>() {
  return consume<Types.Payload>();
}
export function inferred() {
  return consume<unknown extends infer Payload ? Payload : never>();
}
export function mapped() {
  return consume<{ [Payload in 'x']: Payload }>();
}
export function aliased() {
  type Payload = number;
  return 1 as Payload;
}
export function declared() {
  interface Local {}
  return 1 as unknown as Local;
}
export class Holder<Payload> {
  value!: Payload;
  item!: Item;
  echo = (x: Types.Payload): Payload => x;
}
{ class Hidden {} }
export function hidden() { return null as unknown as Hidden; }
";
    let file = extract("src/generic.ts", source);
    assert_reference(&file, "q", (ReferenceKind::TypeOf, "Types.Payload"));
    assert_reference(&file, "declared", (ReferenceKind::TypeOf, "Local"));
    assert_reference(&file, "Holder::item", (ReferenceKind::TypeOf, "Item"));
    assert_reference(
        &file,
        "Holder::echo",
        (ReferenceKind::TypeOf, "Types.Payload"),
    );
    assert!(
        references_owned_by(&file, "hidden")
            .all(|reference| reference.kind != ReferenceKind::TypeOf || reference.name != "Hidden"),
        "a class confined to another block is not the asserted type: {:?}",
        file.references
    );
    for owner in [
        "f",
        "g",
        "k",
        "Box::open",
        "inferred",
        "mapped",
        "aliased",
        "Holder::value",
        "Holder::echo",
    ] {
        assert!(
            references_owned_by(&file, owner).all(|reference| !matches!(
                reference.kind,
                ReferenceKind::TypeOf | ReferenceKind::Returns
            ) || !matches!(
                reference.name.as_str(),
                "Payload" | "Item"
            )),
            "{owner}: a type parameter named a declared type: {:?}",
            file.references
        );
    }
    assert_reference(&file, "h", (ReferenceKind::TypeOf, "Payload"));
}

#[test]
fn using_resources_and_with_bodies_shadow_outer_bindings() {
    let source = r"
const MAX_RETRY = 10;
function f() {
  const x = 1;
  {
    using x = acquire();
    consume(x);
  }
  return x;
}
function g() {
  with ({ MAX_RETRY: 2 }) {
    return MAX_RETRY;
  }
}
function h() { with ({ x: MAX_RETRY }) {} }
function k() {
  const y = 1;
  with ({ y: 2 }) { use(y); }
  using resource = acquire(y);
  return consume(y);
}
";
    let file = extract("src/resources.js", source);
    assert_eq!(
        def_use_sites(&file)
            .into_iter()
            .filter(|(owner, _, _)| owner == "k")
            .map(|(_, name, line)| (name, line))
            .collect::<Vec<_>>(),
        [("y".to_owned(), 20), ("y".to_owned(), 21)],
        "a `with` body hides the local; a `using` initializer reads it: {:?}",
        file.references
    );
    let f = file
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "f")
        .map(|symbol| symbol.id.clone());
    let sites = file
        .references
        .iter()
        .filter(|reference| reference.owner == f && reference.name == "x")
        .map(|reference| (reference.kind, reference.span.start_line()))
        .collect::<Vec<_>>();
    assert!(
        !sites.iter().any(|(kind, line)| *line == 7
            && matches!(kind, ReferenceKind::References | ReferenceKind::DefUse)),
        "a `using` resource read named the outer local: {sites:?}"
    );
    assert_eq!(
        constant_reads(&file),
        vec![("h".to_owned(), "MAX_RETRY".to_owned(), 16)],
        "a `with` body may resolve the name against its object, its operand cannot: {:?}",
        file.references
    );
}

fn def_use_sites(file: &ExtractedFile) -> Vec<(String, String, u32)> {
    file.references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::DefUse)
        .map(|reference| {
            (
                owner_name(file, reference),
                reference.name.clone(),
                reference.span.start_line(),
            )
        })
        .collect()
}

fn constant_reads(file: &ExtractedFile) -> Vec<(String, String, u32)> {
    file.references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::References
                && reference.owner.is_some()
                && reference
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        })
        .map(|reference| {
            (
                owner_name(file, reference),
                reference.name.clone(),
                reference.span.start_line(),
            )
        })
        .collect()
}

fn decorators_of(file: &ExtractedFile, owner: &str) -> Vec<String> {
    references_owned_by(file, owner)
        .filter(|reference| reference.kind == ReferenceKind::Decorates)
        .map(|reference| reference.name.clone())
        .collect()
}

fn owner_name(file: &ExtractedFile, reference: &ExtractedReference) -> String {
    reference
        .owner
        .as_ref()
        .and_then(|owner| file.symbols.iter().find(|symbol| &symbol.id == owner))
        .map_or_else(String::new, |symbol| symbol.qualified_name.clone())
}

fn references_owned_by<'file>(
    file: &'file ExtractedFile,
    owner: &str,
) -> impl Iterator<Item = &'file ExtractedReference> {
    let ids = file
        .symbols
        .iter()
        .filter(|symbol| symbol.qualified_name == owner)
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    file.references.iter().filter(move |reference| {
        reference
            .owner
            .as_ref()
            .is_some_and(|reference_owner| ids.contains(reference_owner))
    })
}

fn assert_reference(file: &ExtractedFile, owner: &str, (kind, name): (ReferenceKind, &str)) {
    assert!(
        references_owned_by(file, owner)
            .any(|reference| reference.kind == kind && reference.name == name),
        "missing {kind:?} {name} owned by {owner}: {:?}",
        file.references
    );
}

fn symbol<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("missing {kind:?} {qualified_name}: {:#?}", names(file)))
}

fn names(file: &ExtractedFile) -> Vec<(SymbolKind, &str)> {
    file.symbols
        .iter()
        .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
        .collect()
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("parity snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("parity extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("parity extraction failed: {error}"))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("parity source limit failed: {error}"))
}

/// A Stripe-shaped live key assembled at runtime, so no complete key-shaped
/// literal sits in the source (secret scanners flag those).
fn stripe_canary(body: &str) -> String {
    ["sk", "live", body].join("_")
}
