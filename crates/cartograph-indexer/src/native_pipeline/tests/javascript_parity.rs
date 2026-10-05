//! Cross-file resolution of the JavaScript/TypeScript parity facts: def-use
//! locals, decorators, class fields and heritage, constant reads, binding
//! tables, and inline import types resolve through the existing lexical and
//! import-binding resolvers, deterministically and without name guessing.

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EXACT_LEXICAL_PROVENANCE, EdgeKind,
    IMPORT_BINDING_PROVENANCE, ReferenceKind, SymbolInput, build_capability_generation,
    capability_symbol, capability_symbol_by,
};

fn generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reversed = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reversed.digest());
    assert_eq!(forward.references(), reversed.references());
    assert_eq!(forward.edges(), reversed.edges());
    forward
}

fn has_edge(
    facts: &CanonicalGenerationFacts,
    (source, target): (&SymbolInput, &SymbolInput),
    kind: EdgeKind,
) -> bool {
    facts.edges().iter().any(|edge| {
        edge.source_symbol_id == source.symbol_id
            && edge.target_symbol_id == target.symbol_id
            && edge.kind == kind
    })
}

#[test]
fn def_use_sites_resolve_only_to_the_owning_callables_local() {
    let facts = generation(&[
        (
            "src/defuse.ts",
            "export const x = 0;\nexport function f() { let x = 1; console.log(x); return x; }\nexport function loops() { for (let i = 0; i < 2; i++) {} for (let i = 0; i < 3; i++) {} }\n",
        ),
        (
            "src/other.ts",
            "export function g() { let x = 2; return x; }\n",
        ),
    ]);
    let f = capability_symbol(&facts, "src/defuse.ts", "f");
    let local = capability_symbol(&facts, "src/defuse.ts", "f::x");
    let module_x = capability_symbol(&facts, "src/defuse.ts", "x");
    let other_local = capability_symbol(&facts, "src/other.ts", "g::x");

    let sites = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&f.symbol_id)
                && reference.reference_kind == ReferenceKind::DefUse.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 2, "{sites:?}");
    for site in sites {
        assert_eq!(site.reference_name, "x");
        assert_eq!(site.target_symbol_id.as_ref(), Some(&local.symbol_id));
        assert_eq!(site.resolution_provenance, EXACT_LEXICAL_PROVENANCE);
    }
    assert!(has_edge(&facts, (f, local), EdgeKind::DefUse));
    for wrong in [module_x, other_local] {
        assert!(
            !has_edge(&facts, (f, wrong), EdgeKind::DefUse),
            "def-use escaped the callable scope to {}",
            wrong.qualified_name
        );
    }

    let loops = capability_symbol(&facts, "src/defuse.ts", "loops");
    let repeated = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&loops.symbol_id)
                && reference.reference_kind == ReferenceKind::DefUse.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(repeated.len(), 4, "{repeated:?}");
    assert!(
        repeated
            .iter()
            .all(|reference| reference.target_symbol_id.is_none()),
        "a name declared twice in one callable must stay ambiguous: {repeated:?}"
    );
}

#[test]
fn decorators_resolve_through_named_and_namespace_imports() {
    let facts = generation(&[
        (
            "src/decorators.ts",
            "export function Get(path: string) { return (t: unknown) => t; }\nexport function Injectable() { return (t: unknown) => t; }\n",
        ),
        (
            "src/ng.ts",
            "export function Input() { return (t: unknown) => t; }\n",
        ),
        (
            "src/controller.ts",
            "import { Get, Injectable } from './decorators';\nimport * as ng from './ng';\n@Injectable()\nexport class Controller {\n  @Get('/items') list() { return []; }\n  @ng.Input() value: string;\n}\n",
        ),
    ]);
    for (owner, name, path, target) in [
        (
            "Controller",
            "Injectable",
            "src/decorators.ts",
            "Injectable",
        ),
        ("Controller::list", "Get", "src/decorators.ts", "Get"),
        ("Controller::value", "ng.Input", "src/ng.ts", "Input"),
    ] {
        let owner = capability_symbol(&facts, "src/controller.ts", owner);
        let target = capability_symbol(&facts, path, target);
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named(name, ReferenceKind::Decorates);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert!(has_edge(&facts, (owner, target), EdgeKind::Decorates));
    }
}

#[test]
fn class_field_types_and_plain_javascript_bases_resolve_across_files() {
    let facts = generation(&[
        ("src/model.ts", "export interface Repo { find(): void }\n"),
        ("src/base.js", "export class Base {}\n"),
        (
            "src/service.ts",
            "import { Repo } from './model';\nexport class Service {\n  repo: Repo;\n  handler = (repo: Repo) => repo.find();\n}\n",
        ),
        (
            "src/child.js",
            "import { Base } from './base.js';\nexport class Child extends Base {}\n",
        ),
    ]);
    let repo = capability_symbol(&facts, "src/model.ts", "Repo");
    for owner in ["Service::repo", "Service::handler"] {
        let owner = capability_symbol(&facts, "src/service.ts", owner);
        assert!(
            has_edge(&facts, (owner, repo), EdgeKind::TypeOf),
            "{}",
            owner.qualified_name
        );
    }
    let child = capability_symbol(&facts, "src/child.js", "Child");
    let base = capability_symbol(&facts, "src/base.js", "Base");
    assert!(has_edge(&facts, (child, base), EdgeKind::Extends));
}

#[test]
fn constant_reads_and_binding_tables_resolve_module_values() {
    let facts = generation(&[
        (
            "src/limits.ts",
            "export const IMPORTED_LIMIT = 5;\nexport function handlerA() {}\nexport function handlerB() {}\n",
        ),
        (
            "src/use.ts",
            "import { IMPORTED_LIMIT, handlerA, handlerB } from './limits';\nconst MAX_RETRY = 3;\nexport function run(n: number) { return n > MAX_RETRY ? IMPORTED_LIMIT : 0; }\nexport const ROUTES = { a: handlerA, list: [handlerB] };\n",
        ),
    ]);
    let run = capability_symbol(&facts, "src/use.ts", "run");
    let max_retry = capability_symbol(&facts, "src/use.ts", "MAX_RETRY");
    let imported_limit = capability_symbol(&facts, "src/limits.ts", "IMPORTED_LIMIT");
    assert!(has_edge(&facts, (run, max_retry), EdgeKind::References));
    assert!(has_edge(
        &facts,
        (run, imported_limit),
        EdgeKind::References
    ));

    let routes = capability_symbol(&facts, "src/use.ts", "ROUTES");
    for handler in ["handlerA", "handlerB"] {
        let target = capability_symbol(&facts, "src/limits.ts", handler);
        let reference =
            CapabilityReferenceQuery::new(&facts, routes).named(handler, ReferenceKind::References);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }
}

#[test]
fn inline_import_types_resolve_per_site_without_cross_binding() {
    let facts = generation(&[
        ("src/a.ts", "export interface Options { a: string }\n"),
        ("src/b.ts", "export interface Options { b: string }\n"),
        (
            "src/consumer.ts",
            "export function first(opts?: import('./a').Options) { return opts; }\nexport function second(opts?: import('./b').Options) { return opts; }\n",
        ),
    ]);
    for (owner, path) in [("first", "src/a.ts"), ("second", "src/b.ts")] {
        let owner = capability_symbol(&facts, "src/consumer.ts", owner);
        let target = capability_symbol(&facts, path, "Options");
        let reference =
            CapabilityReferenceQuery::new(&facts, owner).named("Options", ReferenceKind::TypeOf);
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{} bound the wrong inline import",
            owner.qualified_name
        );
        assert!(has_edge(&facts, (owner, target), EdgeKind::TypeOf));
    }
}

#[test]
fn inline_import_types_never_fall_back_or_capture_ordinary_names() {
    let facts = generation(&[
        ("src/real.ts", "export interface Options { real: string }\n"),
        (
            "src/other.ts",
            "export interface Options { other: string }\n",
        ),
        (
            "src/consumer.ts",
            "import { Options } from './real';\ninterface Local {}\nexport function both(o: Options, p: import('./other').Options) { return [o, p]; }\nexport function external(x: import('unavailable-package').Local) { return x; }\nexport function returned(): import('./other').Options { return {} as never; }\n",
        ),
    ]);
    let both = capability_symbol(&facts, "src/consumer.ts", "both");
    let real = capability_symbol(&facts, "src/real.ts", "Options");
    let other = capability_symbol(&facts, "src/other.ts", "Options");
    let targets = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&both.symbol_id)
                && reference.reference_kind == ReferenceKind::TypeOf.as_str()
                && reference.reference_name == "Options"
        })
        .map(|reference| reference.target_symbol_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        [Some(real.symbol_id.clone()), Some(other.symbol_id.clone())],
        "the inline import made the genuine import ambiguous or crossed sites"
    );

    let external = capability_symbol(&facts, "src/consumer.ts", "external");
    let local = capability_symbol(&facts, "src/consumer.ts", "Local");
    let unavailable =
        CapabilityReferenceQuery::new(&facts, external).named("Local", ReferenceKind::TypeOf);
    assert_eq!(
        unavailable.target_symbol_id, None,
        "an unavailable inline import must not fall back to a same-named local"
    );
    assert!(!has_edge(&facts, (external, local), EdgeKind::TypeOf));

    let returned = capability_symbol(&facts, "src/consumer.ts", "returned");
    for kind in [EdgeKind::Returns, EdgeKind::TypeOf] {
        assert!(has_edge(&facts, (returned, other), kind), "{kind:?}");
    }
}

#[test]
fn walked_and_declared_inline_import_types_stay_site_only() {
    let facts = generation(&[
        ("src/real.ts", "export interface Options { real: string }\n"),
        (
            "src/other.ts",
            "export interface Options { other: string }\n",
        ),
        (
            "src/consumer.ts",
            "import { Options } from './real';\ninterface Local {}\nexport type Aliased = import('./other').Options;\nexport type Queried = typeof import('./other').Options;\nexport function use(o: Options) { return o; }\nexport const missing: import('unavailable-package').Local = make();\n",
        ),
    ]);
    let real = capability_symbol(&facts, "src/real.ts", "Options");
    let other = capability_symbol(&facts, "src/other.ts", "Options");
    let use_site = capability_symbol(&facts, "src/consumer.ts", "use");
    let genuine =
        CapabilityReferenceQuery::new(&facts, use_site).named("Options", ReferenceKind::TypeOf);
    assert_eq!(
        genuine.target_symbol_id.as_ref(),
        Some(&real.symbol_id),
        "a walked alias import type made the genuine import ambiguous"
    );
    let aliased = capability_symbol(&facts, "src/consumer.ts", "Aliased");
    assert!(has_edge(&facts, (aliased, other), EdgeKind::TypeOf));

    let missing = capability_symbol(&facts, "src/consumer.ts", "missing");
    let local = capability_symbol(&facts, "src/consumer.ts", "Local");
    let unavailable =
        CapabilityReferenceQuery::new(&facts, missing).named("Local", ReferenceKind::TypeOf);
    assert_eq!(unavailable.target_symbol_id, None);
    assert!(!has_edge(&facts, (missing, local), EdgeKind::TypeOf));
}

#[test]
fn class_field_reads_resolve_to_the_declaring_class_field() {
    let facts = generation(&[(
        "src/counter.ts",
        "export class Counter {\n  private count = 0;\n  #step = 1;\n  next() { this.count += this.#step; return this.count; }\n}\n",
    )]);
    let next = capability_symbol(&facts, "src/counter.ts", "Counter::next");
    for field in ["count", "#step"] {
        let target = capability_symbol(&facts, "src/counter.ts", &format!("Counter::{field}"));
        assert!(
            facts.references().iter().any(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&next.symbol_id)
                    && reference.reference_kind == ReferenceKind::FieldAccess.as_str()
                    && reference.reference_name == field
                    && reference.target_symbol_id.as_ref() == Some(&target.symbol_id)
            }),
            "this.{field} did not resolve to its field: {:?}",
            facts.references()
        );
        assert!(has_edge(&facts, (next, target), EdgeKind::FieldAccess));
    }
}

#[test]
fn class_field_method_inline_import_types_resolve() {
    let facts = generation(&[
        ("src/types.ts", "export interface Foo { a: string }\n"),
        (
            "src/c.ts",
            "export class C {\n  run = (x: import('./types').Foo = make()) => use(x);\n}\n",
        ),
    ]);
    let run = capability_symbol(&facts, "src/c.ts", "C::run");
    let foo = capability_symbol(&facts, "src/types.ts", "Foo");
    let reference = CapabilityReferenceQuery::new(&facts, run).named("Foo", ReferenceKind::TypeOf);
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&foo.symbol_id));
    assert!(has_edge(&facts, (run, foo), EdgeKind::TypeOf));
}

#[test]
fn nested_function_value_reads_resolve_to_the_reading_scopes_function() {
    // `toggle` is bound only in nested scopes: a local arrow function in
    // `useToggle` and a destructured local in `Button`. The value read in
    // `useToggle`'s returned array binds its own local function, never the
    // other scope's namesake.
    let facts = generation(&[(
        "src/toggle.tsx",
        "export function useToggle(initial: boolean) {\n  const toggle = () => initial;\n  return [initial, toggle];\n}\nexport function Button() {\n  const [open, toggle] = useToggle(false);\n  return consume(open, toggle);\n}\n",
    )]);
    let use_toggle = capability_symbol(&facts, "src/toggle.tsx", "useToggle");
    let local = capability_symbol(&facts, "src/toggle.tsx", "useToggle::toggle");
    let read = CapabilityReferenceQuery::new(&facts, use_toggle)
        .named("toggle", ReferenceKind::References);
    assert_eq!(read.target_symbol_id.as_ref(), Some(&local.symbol_id));
}

#[test]
fn nested_value_reads_in_component_scripts_never_bind_the_file_component() {
    // Each file declares a component named after it. A value read of a
    // nested function sharing that name (bound in several scopes in
    // `handler.vue`, once in `solo.svelte`) binds the nested function, not
    // the same-named component the resolver's exact-name lookup would
    // otherwise find first.
    let facts = generation(&[
        (
            "src/handler.vue",
            "<script>\nfunction outer() { function handler() {} return [handler]; }\nfunction other() { const handler = 0; return [handler]; }\n</script>\n",
        ),
        (
            "src/solo.svelte",
            "<script>\nfunction outer() { function solo() {} return [solo]; }\n</script>\n",
        ),
    ]);
    for (path, name) in [("src/handler.vue", "handler"), ("src/solo.svelte", "solo")] {
        let outer = capability_symbol(&facts, path, "outer");
        let nested = capability_symbol(&facts, path, &format!("outer::{name}"));
        let read =
            CapabilityReferenceQuery::new(&facts, outer).named(name, ReferenceKind::References);
        assert_eq!(
            read.target_symbol_id.as_ref(),
            Some(&nested.symbol_id),
            "{path}"
        );
    }
}

#[test]
fn nested_value_reads_keep_the_scope_walk_when_a_namesake_shares_the_qualified_name() {
    // The object method `outer` declares a local overload signature
    // `handler`, and the class `outer` a method of the same qualified name
    // `outer::handler`. The read names the local declaration, which only the
    // resolver's scope walk from the read's owner tells apart from the
    // namesake implementation an exact qualified-name lookup would prefer.
    let path = "src/overload.ts";
    let facts = generation(&[(
        path,
        "register({ outer() {\n  function handler(): void;\n  return [handler];\n} });\nclass outer { handler() {} }\n",
    )]);
    let file_id = &facts
        .files()
        .iter()
        .find(|file| file.normalized_path == path)
        .unwrap_or_else(|| panic!("missing {path}"))
        .file_id;
    let symbol = |kind: &str, qualified_name: &str| {
        capability_symbol_by(&facts, file_id, |symbol| {
            symbol.symbol_kind == kind && symbol.qualified_name == qualified_name
        })
    };
    let owner = symbol("method", "outer");
    let local = symbol("function", "outer::handler");
    let read =
        CapabilityReferenceQuery::new(&facts, owner).named("handler", ReferenceKind::References);
    assert_eq!(read.target_symbol_id.as_ref(), Some(&local.symbol_id));
}

#[test]
fn nested_value_reads_bind_the_local_past_a_file_component_and_a_qualified_namesake() {
    // In `handler.vue` the file component `handler`, the class method
    // `outer::handler`, and the object method `outer`'s local function
    // `outer::handler` all answer the read of `handler` in the object method
    // by an exact name: the component by its bare name, the class method by
    // the local's qualified name. Only the scope walk from the read's owner
    // binds the local function, with or without another scope's namesake
    // local (which makes the name bound in several nested scopes).
    let class_and_local = "<script>\nregister({\n  outer() {\n    function handler() {}\n    return [handler];\n  }\n});\nclass outer {\n  handler() {}\n}\n";
    for elsewhere in ["", "function elsewhere() {\n  const handler = 0;\n}\n"] {
        let path = "src/handler.vue";
        let source = format!("{class_and_local}{elsewhere}</script>\n");
        let facts = generation(&[(path, source.as_str())]);
        let file_id = &facts
            .files()
            .iter()
            .find(|file| file.normalized_path == path)
            .unwrap_or_else(|| panic!("missing {path}"))
            .file_id;
        let symbol = |kind: &str, qualified_name: &str| {
            capability_symbol_by(&facts, file_id, |symbol| {
                symbol.symbol_kind == kind && symbol.qualified_name == qualified_name
            })
        };
        let owner = symbol("method", "outer");
        let local = symbol("function", "outer::handler");
        let read = CapabilityReferenceQuery::new(&facts, owner)
            .named("handler", ReferenceKind::References);
        assert_eq!(
            read.target_symbol_id.as_ref(),
            Some(&local.symbol_id),
            "elsewhere: {elsewhere:?}"
        );
        assert_eq!(
            read.resolution_provenance, EXACT_LEXICAL_PROVENANCE,
            "elsewhere: {elsewhere:?}"
        );
    }
}

#[test]
fn value_reads_of_module_bindings_resolve_past_other_scopes_namesakes() {
    let facts = generation(&[(
        "src/values.ts",
        "export const config = {};\nexport class C { run = (config: object) => config; }\nexport function f() { return consume(config); }\nexport function k(single: object) { return consume(single); }\n",
    )]);
    let module_config = capability_symbol(&facts, "src/values.ts", "config");
    let f = capability_symbol(&facts, "src/values.ts", "f");
    let read = CapabilityReferenceQuery::new(&facts, f).named("config", ReferenceKind::References);
    assert_eq!(
        read.target_symbol_id.as_ref(),
        Some(&module_config.symbol_id)
    );
    let k = capability_symbol(&facts, "src/values.ts", "k");
    let parameter = capability_symbol(&facts, "src/values.ts", "k::single");
    let read = CapabilityReferenceQuery::new(&facts, k).named("single", ReferenceKind::References);
    assert_eq!(read.target_symbol_id.as_ref(), Some(&parameter.symbol_id));
}

#[test]
fn shadowed_reads_never_bind_to_module_or_cross_file_namesakes() {
    // A local, a destructured require, another function's parameter, and a
    // parameter-rebound decorator receiver each hide the module or imported
    // namesake; no edge may reach it. Unshadowed controls still resolve.
    let facts = generation(&[
        (
            "src/ng.ts",
            "export function Input() { return (t: unknown) => t; }\n",
        ),
        (
            "src/ui.js",
            "export const shared = {};\nexport function handler() {}\n",
        ),
        (
            "src/limits.js",
            "export const MAX_RETRY = 10;\nexport function local() { let MAX_RETRY = 2; return MAX_RETRY; }\nexport function plain() { return MAX_RETRY; }\n",
        ),
        (
            "src/values.js",
            "class C { handler() {} }\nfunction own(handler) { return handler; }\nexport function use() { return consume(handler); }\nconst shared = {};\nexport function required() { const { shared } = require('./ui'); return consume(shared); }\nexport function direct() { return consume(shared); }\n",
        ),
        (
            "src/decorated.ts",
            "import * as ng from './ng';\nexport function make(ng: any) {\n  @ng.Input()\n  class Local {}\n  return Local;\n}\n@ng.Input()\nexport class Module {}\n",
        ),
    ]);
    let max_retry = capability_symbol(&facts, "src/limits.js", "MAX_RETRY");
    let local = capability_symbol(&facts, "src/limits.js", "local");
    let plain = capability_symbol(&facts, "src/limits.js", "plain");
    assert!(!has_edge(&facts, (local, max_retry), EdgeKind::References));
    assert!(has_edge(&facts, (plain, max_retry), EdgeKind::References));

    let ui_handler = capability_symbol(&facts, "src/ui.js", "handler");
    let ui_shared = capability_symbol(&facts, "src/ui.js", "shared");
    let use_ = capability_symbol(&facts, "src/values.js", "use");
    let required = capability_symbol(&facts, "src/values.js", "required");
    let direct = capability_symbol(&facts, "src/values.js", "direct");
    let module_shared = capability_symbol(&facts, "src/values.js", "shared");
    let own_parameter = capability_symbol(&facts, "src/values.js", "own::handler");
    for wrong in [ui_handler, own_parameter] {
        assert!(
            !has_edge(&facts, (use_, wrong), EdgeKind::References),
            "another function's parameter read reached {}",
            wrong.qualified_name
        );
    }
    for wrong in [module_shared, ui_shared] {
        assert!(!has_edge(&facts, (required, wrong), EdgeKind::References));
    }
    assert!(has_edge(
        &facts,
        (direct, module_shared),
        EdgeKind::References
    ));

    let input = capability_symbol(&facts, "src/ng.ts", "Input");
    let shadowed = capability_symbol(&facts, "src/decorated.ts", "make::Local");
    let module = capability_symbol(&facts, "src/decorated.ts", "Module");
    assert!(!has_edge(&facts, (shadowed, input), EdgeKind::Decorates));
    assert!(has_edge(&facts, (module, input), EdgeKind::Decorates));
}

#[test]
fn constant_reads_of_imported_locals_resolve_to_the_imported_value() {
    let facts = generation(&[
        ("src/migrations.js", "export const SCHEMA_VERSION = 3;\n"),
        (
            "src/admin.js",
            "export async function migrate() {\n  const { SCHEMA_VERSION } = await import('./migrations.js');\n  return SCHEMA_VERSION;\n}\n",
        ),
    ]);
    let migrate = capability_symbol(&facts, "src/admin.js", "migrate");
    let version = capability_symbol(&facts, "src/migrations.js", "SCHEMA_VERSION");
    assert!(has_edge(&facts, (migrate, version), EdgeKind::References));
}

#[test]
fn local_constants_shadowing_imports_resolve_to_the_local() {
    // A local declaration answers before an import of the same name, so the
    // read resolves to the local; a read outside the block that declares a
    // namesake is not recorded at all.
    let facts = generation(&[
        ("src/limits.js", "export const LIMIT_A = 5;\n"),
        (
            "src/use.js",
            "import { LIMIT_A } from './limits.js';\nexport function local() { const LIMIT_A = 1; return LIMIT_A; }\nexport function blocked(flag) { if (flag) { const LIMIT_A = 2; } return LIMIT_A; }\nexport function plain() { return LIMIT_A; }\n",
        ),
    ]);
    let imported = capability_symbol(&facts, "src/limits.js", "LIMIT_A");
    let local = capability_symbol(&facts, "src/use.js", "local");
    let local_constant = capability_symbol(&facts, "src/use.js", "local::LIMIT_A");
    let blocked = capability_symbol(&facts, "src/use.js", "blocked");
    let blocked_constant = capability_symbol(&facts, "src/use.js", "blocked::LIMIT_A");
    let plain = capability_symbol(&facts, "src/use.js", "plain");
    assert!(has_edge(
        &facts,
        (local, local_constant),
        EdgeKind::References
    ));
    assert!(!has_edge(&facts, (local, imported), EdgeKind::References));
    assert!(!has_edge(
        &facts,
        (blocked, blocked_constant),
        EdgeKind::References
    ));
    assert!(has_edge(&facts, (plain, imported), EdgeKind::References));
}
