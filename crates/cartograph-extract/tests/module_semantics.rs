//! Integration coverage for Cartograph native extraction contracts.

mod credential_support;
mod dependency_ownership;
#[path = "credential_support/escaped_names.rs"]
mod escaped_names;
#[path = "credential_support/escaped_specifiers.rs"]
mod escaped_specifiers;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractedFile, ImportBindingKind, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn angle_bracket_casts_keep_awaited_dynamic_import_selected_aliases() {
    let extracted = extract(
        "src/casts.ts",
        r"
export async function load() {
  const selected = (<Module> /* value */ await import(/* module */ './m')).catch;
  import(<string> /* path */ './path');
  return selected;
}
",
    );
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Named
            && binding.module_specifier == "./m"
            && binding.imported_name == "catch"
            && binding.local_name == "selected"
    }));
    assert_one_module_site(&extracted, "./m", Some("selected"));
    assert_one_module_site(&extracted, "./path", Some("load"));
}

#[test]
fn dynamic_import_promise_methods_keep_only_the_module_reference() {
    let extracted = extract(
        "src/promises.ts",
        r"
import(/* site */ './then' /* end */).then(module => module.Widget);
(import(/* site */ './catch')).catch(handleError);
(import(/* site */ './finally' /* end */) as Promise<unknown>)!.finally(cleanup);
import('./optional')?.then(module => module.Widget).catch(handleError).finally(cleanup);
const then = import('./bound-then').then;
const caught = (import('./bound-catch')).catch;
const done = import('./bound-finally')!.finally;
",
    );
    for (module, owner) in [
        ("./then", None),
        ("./catch", None),
        ("./finally", None),
        ("./optional", None),
        ("./bound-then", Some("then")),
        ("./bound-catch", Some("caught")),
        ("./bound-finally", Some("done")),
    ] {
        assert_one_module_site(&extracted, module, owner);
    }
    assert_eq!(extracted.import_bindings, []);
}

#[test]
fn awaited_dynamic_import_members_preserve_bindings_including_promise_names() {
    let extracted = extract(
        "src/awaited.ts",
        r"
export async function load() {
  const selected = (await import('./target')).member;
  const wrapped = ((await import('./target')) as Module).other;
  const { value, renamed: alias } = await import('./target');
  (await import('./target')).then();
  (await import('./target')).catch();
  (await import('./target')).finally();
}
type Then = import('./types').then;
",
    );
    for (imported, local) in [
        ("member", "selected"),
        ("other", "wrapped"),
        ("value", "value"),
        ("renamed", "alias"),
        ("then", "then"),
        ("catch", "catch"),
        ("finally", "finally"),
    ] {
        assert!(
            extracted.import_bindings.iter().any(|binding| {
                binding.kind == ImportBindingKind::Named
                    && binding.module_specifier == "./target"
                    && binding.imported_name == imported
                    && binding.local_name == local
            }),
            "missing awaited {imported} as {local}: {:?}",
            extracted.import_bindings
        );
    }
    let references = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Imports && reference.name == "./target"
        })
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 6);
    for (reference, owner) in references
        .into_iter()
        .zip(["selected", "wrapped", "load", "load", "load", "load"])
    {
        assert_eq!(
            extracted
                .references
                .iter()
                .filter(|other| {
                    other.kind == ReferenceKind::Imports && other.span == reference.span
                })
                .count(),
            1
        );
        assert_eq!(
            reference.owner,
            extracted
                .symbols
                .iter()
                .find(|symbol| symbol.name == owner)
                .map(|symbol| symbol.id.clone())
        );
    }
    assert_one_module_site(&extracted, "./types", Some("Then"));
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.module_specifier == "./types"
            && binding.imported_name == "then"
            && binding.local_name == "import()"
    }));
}

#[test]
fn dynamic_import_promise_destructuring_preserves_awaited_module_members() {
    let extracted = extract(
        "src/destructuring.ts",
        r"
const { then, catch: reject, finally: finish } = import('./promise');
export async function load() {
  const { then: ready, catch: recover, finally: complete } = await import('./module');
  return [ready, recover, complete];
}
",
    );
    assert_one_module_site(&extracted, "./promise", None);
    assert_one_module_site(&extracted, "./module", Some("load"));
    assert!(
        extracted
            .import_bindings
            .iter()
            .all(|binding| binding.module_specifier != "./promise")
    );
    for (imported, local) in [
        ("then", "ready"),
        ("catch", "recover"),
        ("finally", "complete"),
    ] {
        assert!(extracted.import_bindings.iter().any(|binding| {
            binding.kind == ImportBindingKind::Named
                && binding.module_specifier == "./module"
                && binding.imported_name == imported
                && binding.local_name == local
        }));
    }
}

#[test]
fn dynamic_import_nonbinding_patterns_preserve_module_and_member_references() {
    let extracted = extract(
        "src/patterns.ts",
        r"
export async function load() {
  const { value } = (await import('./nested')).group;
  const [first] = await import('./array');
  return [value, first];
}
",
    );
    for module in ["./nested", "./array"] {
        assert_one_module_site(&extracted, module, Some("load"));
    }
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.module_specifier == "./nested"
            && binding.imported_name == "group"
            && binding.local_name == "group"
    }));
}

fn assert_one_module_site(extracted: &ExtractedFile, module: &str, owner: Option<&str>) {
    let references = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Imports && reference.name == module)
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 1, "module load {module}");
    let expected = owner.map(|name| {
        extracted
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing owner {name}"))
            .id
            .clone()
    });
    assert_eq!(references[0].owner, expected, "module owner {module}");
}

#[test]
fn dynamic_import_bindings_are_ast_native_and_preserve_original_names() {
    let source = r"
export async function load() {
  const { foo, bar: alias } = await import('./target.js');
  const module = await import('./target.js');
  module.qux();
  const inline = import('./target.js').direct;
  return [foo, alias, inline];
}
type Loaded = import('./types.js').Widget;
";
    let first = extract("src/consumer.ts", source);
    let second = extract("src/consumer.ts", source);
    assert_eq!(first, second);

    for (module, imported, local, kind) in [
        ("./target.js", "foo", "foo", ImportBindingKind::Named),
        ("./target.js", "bar", "alias", ImportBindingKind::Named),
        ("./target.js", "*", "module", ImportBindingKind::Namespace),
        ("./target.js", "direct", "inline", ImportBindingKind::Named),
        // An inline import type binds only its own site: its local name is
        // no identifier, so it never captures another use of `Widget`.
        ("./types.js", "Widget", "import()", ImportBindingKind::Named),
    ] {
        assert!(
            first.import_bindings.iter().any(|binding| {
                binding.module_specifier == module
                    && binding.imported_name == imported
                    && binding.local_name == local
                    && binding.kind == kind
            }),
            "missing {kind:?} {module}:{imported} as {local}: {:?}",
            first.import_bindings
        );
    }
    for imported in ["foo", "bar", "direct", "Widget"] {
        assert!(
            first.references.iter().any(|reference| {
                reference.name == imported && reference.kind == ReferenceKind::References
            }),
            "missing dynamic imported-name reference {imported}: {:?}",
            first.references
        );
    }
    for module in ["./target.js", "./types.js"] {
        assert!(
            first.references.iter().any(|reference| {
                reference.name == module && reference.kind == ReferenceKind::Imports
            }),
            "missing dynamic module reference {module}: {:?}",
            first.references
        );
    }
}

#[test]
fn computed_dynamic_imports_never_invent_static_bindings() {
    let extracted = extract(
        "src/computed.ts",
        "export async function load(name: string) { const module = await import(name); return module; }\n",
    );
    assert_eq!(extracted.import_bindings, []);
    assert!(extracted.references.iter().all(|reference| {
        reference.name != "import" && reference.kind != ReferenceKind::Imports
    }));
}

#[test]
fn react_lazy_dynamic_import_retains_its_default_consumer_contract() {
    let source = r"
import { lazy } from 'react';
export const Panel = lazy(() => import('./panel'));
";
    let first = extract("src/lazy-panel.ts", source);
    let second = extract("src/lazy-panel.ts", source);
    assert_eq!(first, second);

    assert!(first.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Default
            && binding.module_specifier == "./panel"
            && binding.imported_name == "default"
            && binding.local_name == "default"
    }));
    assert!(first.references.iter().any(|reference| {
        reference.name == "default" && reference.kind == ReferenceKind::References
    }));
}

#[test]
fn type_aliases_preserve_their_bounded_right_hand_side_for_agent_retrieval() {
    let extracted = extract(
        "src/types.ts",
        "export type Identifier = string | { readonly value: number };\n",
    );
    let alias = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::TypeAlias && symbol.name == "Identifier")
        .unwrap_or_else(|| panic!("missing type alias: {:?}", extracted.symbols));
    assert_eq!(
        alias.signature.as_deref(),
        Some("type Identifier = string | { readonly value: number };")
    );
}

#[test]
fn wildcard_and_namespace_reexports_retain_explicit_project_semantics() {
    let extracted = extract(
        "src/barrel.ts",
        "export * from './public.js';\nexport * as tools from './tools.js';\n",
    );
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::ReExportAll
            && binding.module_specifier == "./public.js"
            && binding.imported_name == "*"
            && binding.local_name == "*"
    }));
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::ReExportNamespace
            && binding.module_specifier == "./tools.js"
            && binding.imported_name == "*"
            && binding.local_name == "tools"
    }));
    let namespace = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Export && symbol.qualified_name == "tools")
        .unwrap_or_else(|| panic!("missing namespace export: {:?}", extracted.symbols));
    assert!(namespace.export.exported);
    for module in ["./public.js", "./tools.js"] {
        assert!(extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Imports && reference.name == module
        }));
    }
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("module source limit failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("module snapshot failed: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::TypeScript);
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("module extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("module extraction failed: {error}"))
}

#[test]
fn ordinary_jsdoc_links_retain_exact_documentation() {
    let file = credential_support::extract("doc.ts", "/** See {@link User}. */ function f() {}\n");
    let function = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "f")
        .unwrap_or_else(|| panic!("function f must remain: {file:?}"));
    assert_eq!(function.qualified_name, "f");
    assert_eq!(function.docstring.as_deref(), Some("See {@link User}."));
}

#[test]
fn escaped_quoted_import_and_export_names_abstain_without_losing_safe_modules() {
    for &(path, source) in escaped_names::ESCAPED_NAME_CASES {
        if std::path::Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("ts") || extension.eq_ignore_ascii_case("js")
            })
        {
            let file = credential_support::extract(path, source);
            credential_support::assert_no_credentials(&file);
            assert!(file.import_bindings.iter().all(|binding| {
                binding.imported_name == "*" || binding.imported_name == "default"
            }));
        }
    }
    credential_support::assert_screened(
        "namespace.ts",
        "export * as \"@VALUE@\" from \"./safe\";\n",
        "token",
    );
}

#[test]
fn escaped_javascript_module_operands_abstain_before_import_facts() {
    for template in [
        "import X from \"@VALUE@\";\n",
        "export * from \"@VALUE@\";\n",
        "export { X } from \"@VALUE@\";\n",
        "function f(x: import(\"@VALUE@\").T) {}\n",
        "const x = import(\"@VALUE@\");\n",
        "const x = require(\"@VALUE@\");\n",
    ] {
        escaped_specifiers::assert_escaped_specifiers_abstain("main.ts", template);
    }
    for (path, template) in [
        (
            "main.astro",
            "---\nimport X from \"@VALUE@\";\n---\n<X />\n",
        ),
        (
            "main.vue",
            "<script>import X from \"@VALUE@\";</script><template><X /></template>\n",
        ),
        (
            "main.svelte",
            "<script>import X from \"@VALUE@\";</script><X />\n",
        ),
    ] {
        escaped_specifiers::assert_escaped_specifiers_abstain(path, template);
    }
}

#[test]
fn undecoded_literal_signatures_abstain_without_losing_safe_declarations() {
    for (source, kind, name) in [
        (
            r#"function f(x: import("https:\/\/reader:FAKEPASSWORDxyz@example.invalid/module").T) {}"#,
            SymbolKind::Function,
            "f",
        ),
        (
            r#"import { "sk_l\u0069ve_FAKE1234567890abcdef" as x } from "./safe";"#,
            SymbolKind::Import,
            "./safe",
        ),
        (
            r#"type Safe = "sk_l\u0069ve_FAKE1234567890abcdef";"#,
            SymbolKind::TypeAlias,
            "Safe",
        ),
    ] {
        let file = credential_support::extract("signature.ts", source);
        credential_support::assert_no_credentials(&file);
        let symbol = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == kind && symbol.name == name)
            .unwrap_or_else(|| panic!("safe declaration must remain: {file:?}"));
        assert_eq!(symbol.signature, None);
    }
    for (source, name, signature) in [
        (
            r#"function f(x: import("https://example.invalid/token/module").T) {}"#,
            "f",
            r#"(x: import("https://example.invalid/token/module").T)"#,
        ),
        (
            r#"import { "token" as x } from "./safe";"#,
            "./safe",
            r#"import { "token" as x } from "./safe";"#,
        ),
    ] {
        let file = credential_support::extract("ordinary.ts", source);
        let symbol = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("ordinary declaration must remain: {file:?}"));
        assert_eq!(symbol.signature.as_deref(), Some(signature));
    }
    let file = credential_support::extract(
        "namespace.php",
        r#"<?php function keep(\n\Value $value): \n\Value { echo "\n"; return $value; }"#,
    );
    let function = file
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Function && symbol.name == "keep")
        .unwrap_or_else(|| panic!("namespace signature must remain: {file:?}"));
    assert_eq!(
        function.signature.as_deref(),
        Some(r"(\n\Value $value): \n\Value")
    );
}

#[test]
fn inline_dynamic_and_commonjs_imports_screen_credentials() {
    credential_support::assert_screened(
        "main.ts",
        "function f(x: import(\"@VALUE@\").T) {}\nconst m = import(\"@VALUE@\");\nconst n = require(\"@VALUE@\");\n",
        "https://example.invalid/module",
    );
}
