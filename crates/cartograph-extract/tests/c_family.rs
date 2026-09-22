//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn c_extracts_includes_macros_types_fields_calls_and_type_edges_without_literals() {
    let source = r#"#include <stdio.h>
#define API_KEY "sk_live_secret"
#define ACTIVE_LIMIT OTHER_LIMIT

typedef struct Point {
  int x;
  int y;
} Point;

Point *makePoint(Point *point) {
  helper(point->x);
  return point;
}
"#;
    let extracted = extract("src/point.c", source);

    assert_symbol(&extracted, SymbolKind::Import, "stdio.h", "stdio.h");
    let api_key = assert_symbol(&extracted, SymbolKind::Constant, "API_KEY", "API_KEY");
    assert!(api_key.signature.is_none());
    let active = assert_symbol(
        &extracted,
        SymbolKind::Constant,
        "ACTIVE_LIMIT",
        "ACTIVE_LIMIT",
    );
    assert_eq!(active.signature.as_deref(), Some("= OTHER_LIMIT"));
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.signature.as_deref() != Some("= <redacted>"))
    );
    assert_symbol(&extracted, SymbolKind::Struct, "Point", "Point");
    assert_symbol(&extracted, SymbolKind::Field, "x", "Point::x");
    assert_symbol(&extracted, SymbolKind::Field, "y", "Point::y");
    let make = assert_symbol(&extracted, SymbolKind::Function, "makePoint", "makePoint");
    assert_eq!(make.signature.as_deref(), Some("Point (Point *point)"));
    assert_reference(
        &extracted,
        Some("makePoint"),
        "helper",
        ReferenceKind::Calls,
    );
    assert_reference(
        &extracted,
        Some("makePoint"),
        "x",
        ReferenceKind::FieldAccess,
    );
    assert_reference(
        &extracted,
        Some("makePoint"),
        "Point",
        ReferenceKind::Returns,
    );
    assert!(!format!("{extracted:?}").contains("sk_live_secret"));
    assert_unique_ids(&extracted);
    assert_locked(
        &extracted,
        "ae97b3a55a90092e770534afd380ba2058e46d6bd9c82304d56185127bddcdb9",
    );
}

#[test]
fn cpp_extracts_scoped_methods_visibility_inheritance_aliases_and_member_calls() {
    let source = r"#include <vector>
class Widget : public Base {
public:
  int field;
  int run(int value) { return helper(value) + this->field; }
};

using Alias = Widget;
int Widget::other() { return run(); }
";
    let extracted = extract("src/widget.cpp", source);

    let class = assert_symbol(&extracted, SymbolKind::Class, "Widget", "Widget");
    assert_reference(&extracted, Some("Widget"), "Base", ReferenceKind::Extends);
    let field = assert_symbol(&extracted, SymbolKind::Field, "field", "Widget::field");
    assert_eq!(field.visibility, Some(Visibility::Public));
    let run = assert_symbol(&extracted, SymbolKind::Method, "run", "Widget::run");
    assert_eq!(run.visibility, Some(Visibility::Public));
    assert_eq!(run.signature.as_deref(), Some("int (int value)"));
    assert_symbol(&extracted, SymbolKind::TypeAlias, "Alias", "Alias");
    assert_symbol(&extracted, SymbolKind::Method, "other", "Widget::other");
    assert_reference(
        &extracted,
        Some("Widget::run"),
        "helper",
        ReferenceKind::Calls,
    );
    assert_reference(
        &extracted,
        Some("Widget::run"),
        "field",
        ReferenceKind::FieldAccess,
    );
    assert_reference(
        &extracted,
        Some("Widget::other"),
        "run",
        ReferenceKind::Calls,
    );
    assert!(
        extracted
            .containments
            .iter()
            .any(|edge| edge.parent == class.id && edge.child == run.id)
    );
    assert_unique_ids(&extracted);
    assert_locked(
        &extracted,
        "b2b9514d76098e0caae197390ed882c4a11ddc29a733698b4f9c00360eb8c25d",
    );
}

#[test]
fn c_family_preserves_locked_v1_macro_recovery() {
    let recovered = extract(
        "src/video.c",
        "AX_VIN_GLB_API AX_S32 AX_VIN_Init(AX_VOID) { return 0; }\n",
    );
    let recovered_function = assert_symbol(
        &recovered,
        SymbolKind::Function,
        "AX_VIN_Init",
        "AX_VIN_Init",
    );
    assert!(recovered_function.signature.is_none());
    assert!(
        recovered
            .symbols
            .iter()
            .all(|symbol| symbol.name != "AX_VOID")
    );
    assert!(recovered.references.iter().all(|reference| {
        reference.owner.as_ref() != Some(&recovered_function.id)
            || !((reference.kind == ReferenceKind::Returns && reference.name == "AX_VIN_GLB_API")
                || (reference.kind == ReferenceKind::TypeOf && reference.name == "AX_VOID"))
    }));
}

#[test]
fn c_family_recovers_macro_prefixed_cpp_containers() {
    let cases = [
        (
            "SOME_TEMPLATE_MACRO\nclass MyClass { void method() {} };",
            SymbolKind::Class,
            "MyClass",
        ),
        (
            "NLOHMANN_BASIC_JSON_TPL_DECLARATION\nclass basic_json // NOLINT(some-rule)\n    : public detail::base<T>\n{ void foo() {} };",
            SymbolKind::Class,
            "basic_json",
        ),
        (
            "MY_MACRO\nstruct my_struct { int x; void touch() {} };",
            SymbolKind::Struct,
            "my_struct",
        ),
        (
            "TMPL_MACRO\nclass Container { void push() {} void pop() {} };",
            SymbolKind::Class,
            "Container",
        ),
    ];
    for (source, kind, name) in cases {
        let extracted = extract("include/recovered.hpp", source);
        assert_symbol(&extracted, kind, name, name);
        assert!(
            extracted.symbols.iter().any(|symbol| {
                symbol.kind == SymbolKind::Method
                    && symbol.qualified_name.starts_with(&format!("{name}::"))
            }),
            "recovered {name} lost its member scope: {:?}",
            canonical_facts(&extracted)
        );
    }

    let lowercase = extract("include/lower.hpp", "myMacro\nclass Foo {};\n");
    assert!(
        lowercase
            .symbols
            .iter()
            .all(|symbol| !(symbol.kind == SymbolKind::Class && symbol.name == "Foo"))
    );
    let regular = extract(
        "include/regular.hpp",
        "template<typename T> class Vec { void push() {} };\nclass Simple {};\n",
    );
    assert_symbol(&regular, SymbolKind::Class, "Vec", "Vec");
    assert_symbol(&regular, SymbolKind::Class, "Simple", "Simple");
}

#[test]
fn cpp_call_chains_and_constructions_remain_safe() {
    let calls = extract(
        "src/calls.cpp",
        r#"int use() {
  Helper::log();
  Client::create().commit();
  handlers["sk_live_secret"]();
  (*lookup("sk_live_secret"))();
}
"#,
    );
    assert_reference(&calls, Some("use"), "Helper::log", ReferenceKind::Calls);
    assert_reference(&calls, Some("use"), "Client::create", ReferenceKind::Calls);
    assert_reference(
        &calls,
        Some("use"),
        "Client::create().commit",
        ReferenceKind::Calls,
    );
    assert_reference(&calls, Some("use"), "lookup", ReferenceKind::Calls);
    assert!(!format!("{calls:?}").contains("sk_live_secret"));

    let receiver_calls = extract(
        "src/receiver.cpp",
        r"struct Worker {
  void run() {}
  void dispatch(Worker *worker) {
    worker->run();
    this->run();
  }
};
",
    );
    assert_reference(
        &receiver_calls,
        Some("Worker::dispatch"),
        "worker.run",
        ReferenceKind::Calls,
    );
    assert_reference(
        &receiver_calls,
        Some("Worker::dispatch"),
        "run",
        ReferenceKind::Calls,
    );

    let constructions = extract(
        "src/constructions.cpp",
        r"namespace Demo { template<typename T> class Gadget {}; }
class Widget {};
void build() {
  auto *widget = new Widget();
  auto *gadget = new Demo::Gadget<int>();
  auto *bytes = new char[4];
}
",
    );
    assert_reference(
        &constructions,
        Some("build"),
        "Widget",
        ReferenceKind::Instantiates,
    );
    assert_reference(
        &constructions,
        Some("build"),
        "Demo::Gadget",
        ReferenceKind::Instantiates,
    );
    assert!(constructions.references.iter().all(|reference| {
        reference.kind != ReferenceKind::Instantiates || reference.name != "char"
    }));
}

#[test]
fn c_family_locks_forward_typedef_header_routing_and_doxygen() {
    let typedef = extract(
        "include/client.h",
        "/** Client handle. */\ntypedef struct client client_t;\nclient_t *open_client(void);\n",
    );
    let alias = assert_symbol(&typedef, SymbolKind::TypeAlias, "client_t", "client_t");
    assert_eq!(alias.docstring.as_deref(), Some("Client handle."));
    assert!(
        typedef
            .symbols
            .iter()
            .all(|symbol| !(symbol.kind == SymbolKind::Struct && symbol.name == "client_t"))
    );

    let cpp_header = extract(
        "include/widget.h",
        "namespace Demo { class Widget { public: void run() {} }; }\n",
    );
    assert_eq!(cpp_header.language, SourceLanguage::Cpp);
    assert_symbol(&cpp_header, SymbolKind::Class, "Widget", "Demo::Widget");

    let line_comment = extract(
        "src/comments.c",
        "// not a Doxygen block\nint plain(void) { return 0; }\n",
    );
    let plain = assert_symbol(&line_comment, SymbolKind::Function, "plain", "plain");
    assert_eq!(plain.docstring.as_deref(), Some("not a Doxygen block"));

    let docs = extract(
        "src/doxygen.c",
        "/// First line.\n//! Second line.\nint documented(void) { return 0; }\n\n/// Detached.\n\nint detached(void) { return 0; }\n",
    );
    assert_eq!(
        assert_symbol(&docs, SymbolKind::Function, "documented", "documented")
            .docstring
            .as_deref(),
        Some("First line.\nSecond line.")
    );
    assert!(
        assert_symbol(&docs, SymbolKind::Function, "detached", "detached")
            .docstring
            .is_none()
    );
}

#[test]
fn cuda_kernel_launch_and_host_calls_share_the_c_family_contract() {
    let extracted = extract(
        "kernels/fill.cu",
        r"#include <cuda_runtime.h>
__global__ void fillKernel(float *out) { out[0] = 1.0f; }
void launchFill(float *out) {
  fillKernel<<<1, 32>>>(out);
  cudaDeviceSynchronize();
}
",
    );

    assert_eq!(extracted.language, SourceLanguage::Cuda);
    assert_symbol(&extracted, SymbolKind::Function, "fillKernel", "fillKernel");
    assert_symbol(&extracted, SymbolKind::Function, "launchFill", "launchFill");
    assert_reference(
        &extracted,
        Some("launchFill"),
        "fillKernel",
        ReferenceKind::Calls,
    );
    assert_reference(
        &extracted,
        Some("launchFill"),
        "cudaDeviceSynchronize",
        ReferenceKind::Calls,
    );
    assert_unique_ids(&extracted);
    assert_locked(
        &extracted,
        "84cca5a933522ab09de3a0d592acbd317dfbb7fe26a8ab8f2050c68d31d686f3",
    );
}

#[test]
fn cuda_tile_qualifiers_preserve_declarations_and_call_ownership() {
    let extracted = extract(
        "kernels/tile.cu",
        r"__tile__ void tileHelper(float *out) { out[0] = 1.0f; }
__tile_global__ void tileKernel(float *out) { tileHelper(out); }
",
    );
    assert_eq!(extracted.language, SourceLanguage::Cuda);
    assert_symbol(&extracted, SymbolKind::Function, "tileHelper", "tileHelper");
    assert_symbol(&extracted, SymbolKind::Function, "tileKernel", "tileKernel");
    assert_reference(
        &extracted,
        Some("tileKernel"),
        "tileHelper",
        ReferenceKind::Calls,
    );
    assert_unique_ids(&extracted);
}

#[test]
fn shader_modes_extract_structures_functions_signatures_and_calls() {
    let cases = [
        (
            "shader.frag",
            SourceLanguage::Glsl,
            r"struct Light { vec3 position; float intensity; };
float square(float value) { return value * value; }
vec3 normal(vec3 input) { return normalize(input); }
void main() { square(2.0); }
",
            ["square", "normalize"].as_slice(),
            "a7a37cf341b9bc4d434806fc0ba2c6fc33e85178538d48c5943a41e6473594eb",
        ),
        (
            "shader.hlsl",
            SourceLanguage::Hlsl,
            r"struct VSInput { float4 pos : POSITION; };
float helper(float value) { return value; }
float4 main(float4 pos : POSITION) : SV_Position { return helper(pos.x).xxxx; }
",
            ["helper"].as_slice(),
            "a10342b0f59e98799cb4fe6a9d739d5c7a2e3521b32faf9395b4b34bdccf4ccc",
        ),
    ];

    for (path, language, source, calls, expected_digest) in cases {
        let extracted = extract(path, source);
        assert_eq!(extracted.language, language);
        let functions = extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Function)
            .map(|symbol| symbol.name.as_str())
            .collect::<BTreeSet<_>>();
        assert!(functions.contains("main"), "{path}");
        assert!(
            extracted
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Struct),
            "{path}"
        );
        match language {
            SourceLanguage::Glsl => {
                assert_eq!(
                    assert_symbol(&extracted, SymbolKind::Function, "square", "square")
                        .signature
                        .as_deref(),
                    Some("float (float value)")
                );
                assert_eq!(
                    assert_symbol(&extracted, SymbolKind::Function, "normal", "normal")
                        .signature
                        .as_deref(),
                    Some("vec3 (vec3 input)")
                );
                assert_reference(
                    &extracted,
                    Some("normal"),
                    "normalize",
                    ReferenceKind::Calls,
                );
                assert_reference(&extracted, Some("main"), "square", ReferenceKind::Calls);
            }
            SourceLanguage::Hlsl => {
                assert_eq!(
                    assert_symbol(&extracted, SymbolKind::Function, "helper", "helper")
                        .signature
                        .as_deref(),
                    Some("float (float value)")
                );
                assert_eq!(
                    assert_symbol(&extracted, SymbolKind::Function, "main", "main")
                        .signature
                        .as_deref(),
                    Some("float4 (float4 pos : POSITION)")
                );
                assert_reference(&extracted, Some("main"), "helper", ReferenceKind::Calls);
            }
            _ => panic!("unexpected shader language"),
        }
        for call in calls {
            assert_reference(&extracted, None, call, ReferenceKind::Calls);
        }
        assert_unique_ids(&extracted);
        assert_locked(&extracted, expected_digest);
    }
}

#[test]
fn c_family_bounds_cancellation_nesting_output_and_syntax_damage() {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
        "cancel.c",
        b"void run(void) {}\n",
        limits,
    )
    .unwrap_or_else(|error| panic!("cancellation snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new_for_capability_validation(SourceLanguage::C)
        .unwrap_or_else(|error| panic!("C extractor failed: {error}"));
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || true),
        Err(ExtractError::Cancelled)
    );

    let damaged = extract("broken.c", "void broken( {\n");
    assert_eq!(
        damaged.parse_status,
        cartograph_domain::FileParseStatus::Partial
    );
    assert!(!damaged.diagnostics.is_empty());

    let mut nested = String::from("void deep(void) {\n");
    for _ in 0..300 {
        nested.push_str("if (enabled) {\n");
    }
    for _ in 0..300 {
        nested.push_str("}\n");
    }
    nested.push_str("}\n");
    let recovered = extract_result("deep.c", &nested)
        .unwrap_or_else(|error| panic!("deep C extraction did not recover: {error}"));
    assert_eq!(
        recovered.parse_status,
        cartograph_domain::FileParseStatus::Partial
    );
    assert_eq!(
        recovered.diagnostics[0].code,
        cartograph_extract::DiagnosticCode::NestingLimitExceeded
    );
    assert!(recovered.symbols.is_empty());

    let mut excessive = String::new();
    for index in 0..20_000 {
        excessive.push_str("int value_");
        excessive.push_str(&index.to_string());
        excessive.push_str(";\n");
    }
    assert_eq!(
        extract_result("many.c", &excessive),
        Err(ExtractError::OutputLimit)
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    extract_result(path, source)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}

fn extract_result(path: &str, source: &str) -> Result<ExtractedFile, ExtractError> {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot =
        SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new_for_capability_validation(snapshot.language())?;
    extractor.extract(&snapshot)
}

fn assert_symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
    qualified_name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| {
            symbol.kind == kind && symbol.name == name && symbol.qualified_name == qualified_name
        })
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name}; facts={:?}",
                canonical_facts(extracted)
            )
        })
}

fn assert_reference(
    extracted: &ExtractedFile,
    owner: Option<&str>,
    name: &str,
    kind: ReferenceKind,
) {
    let names = symbol_names(extracted);
    let matched = extracted.references.iter().any(|reference| {
        reference.name == name
            && reference.kind == kind
            && match (owner, reference.owner.as_ref()) {
                (None, _) => true,
                (Some(expected), Some(owner)) => names.get(owner.as_str()) == Some(&expected),
                (Some(_), None) => false,
            }
    });
    assert!(
        matched,
        "missing {kind:?} reference {name}; facts={:?}",
        canonical_facts(extracted)
    );
}

fn assert_unique_ids(extracted: &ExtractedFile) {
    let ids = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), extracted.symbols.len());
}

fn symbol_names(extracted: &ExtractedFile) -> BTreeMap<&str, &str> {
    extracted
        .symbols
        .iter()
        .map(|symbol| (symbol.id.as_str(), symbol.qualified_name.as_str()))
        .collect()
}

fn canonical_facts(extracted: &ExtractedFile) -> Vec<String> {
    let names = symbol_names(extracted);
    let mut facts = vec![format!(
        "F|{}|{:?}|{}|{}",
        extracted.language.as_str(),
        extracted.parse_status,
        extracted.symbols.len(),
        extracted.references.len()
    )];
    facts.extend(extracted.symbols.iter().map(|symbol| {
        format!(
            "S|{}|{}|{}|{}-{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
            symbol.kind.as_str(),
            symbol.name,
            symbol.qualified_name,
            symbol.span.start_byte(),
            symbol.span.end_byte(),
            symbol.structural_digest,
            symbol.signature.as_deref().unwrap_or_default(),
            symbol.docstring.as_deref().unwrap_or_default(),
            symbol.body_search_text,
            symbol.implementation.declaration_only,
            symbol.export.exported,
            symbol.execution.async_symbol,
            symbol.execution.static_member,
            symbol.visibility
        )
    }));
    facts.extend(extracted.containments.iter().map(|edge| {
        format!(
            "C|{}|{}",
            names
                .get(edge.parent.as_str())
                .copied()
                .unwrap_or("<missing>"),
            names
                .get(edge.child.as_str())
                .copied()
                .unwrap_or("<missing>")
        )
    }));
    facts.extend(extracted.references.iter().map(|reference| {
        format!(
            "R|{}|{}|{}|{}-{}",
            reference
                .owner
                .as_ref()
                .and_then(|owner| names.get(owner.as_str()).copied())
                .unwrap_or("<file>"),
            reference.kind.as_str(),
            reference.name,
            reference.span.start_byte(),
            reference.span.end_byte()
        )
    }));
    facts.extend(extracted.import_bindings.iter().map(|binding| {
        format!(
            "I|{:?}|{}|{}|{}|{}-{}",
            binding.kind,
            binding.module_specifier,
            binding.imported_name,
            binding.local_name,
            binding.span.start_byte(),
            binding.span.end_byte()
        )
    }));
    facts.extend(
        extracted
            .diagnostics
            .iter()
            .map(|diagnostic| format!("D|{:?}|{:?}", diagnostic.code, diagnostic.span)),
    );
    facts
}

fn assert_locked(extracted: &ExtractedFile, expected: &str) {
    let facts = canonical_facts(extracted);
    let mut hasher = blake3::Hasher::new_derive_key("cartograph.v2.c-family-corpus.2026-07-24");
    for fact in &facts {
        hasher.update(&u64::try_from(fact.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(fact.as_bytes());
    }
    assert_eq!(
        hasher.finalize().to_hex().as_str(),
        expected,
        "facts={facts:#?}"
    );
}
