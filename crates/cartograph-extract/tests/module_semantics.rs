//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractedFile, ImportBindingKind, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

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
        ("./types.js", "Widget", "Widget", ImportBindingKind::Named),
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
