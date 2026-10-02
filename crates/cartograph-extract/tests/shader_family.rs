//! Shader-family extraction contracts for WGSL, WESL, Slang, and Metal.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn wgsl_extracts_stage_entry_points_bindings_structs_and_module_imports() {
    let source = r"#define_import_path pbr::lighting
#import pbr::common

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> view_projection: mat4x4<f32>;

@group(1) @binding(2)
var base_color_texture: texture_2d<f32>;

fn tone_map(color: vec4<f32>) -> vec4<f32> {
    return color;
}

@vertex
fn vertex_main(@location(0) position: vec3<f32>) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = view_projection * vec4<f32>(position, 1.0);
    return out;
}

@fragment
fn fragment_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return tone_map(in.clip_position);
}

@compute @workgroup_size(8, 8, 1)
fn compute_main() {
}
";
    let extracted = extract("shaders/pbr.wgsl", source);
    assert_eq!(extracted.language, SourceLanguage::Wgsl);

    // Entry points are the top of a real call stack and must be typed by stage,
    // not flattened into "some function" (issue #121).
    for (name, stage) in [
        ("vertex_main", "@vertex"),
        ("fragment_main", "@fragment"),
        ("compute_main", "@compute"),
    ] {
        let entry = symbol(&extracted, SymbolKind::Function, name);
        assert_eq!(
            entry.visibility,
            Some(Visibility::Public),
            "{name} is reachable from the host pipeline"
        );
        let signature = entry
            .signature
            .as_deref()
            .unwrap_or_else(|| panic!("{name} lost its signature"));
        assert!(
            signature.starts_with(stage),
            "{name} must record its pipeline stage, got {signature}"
        );
    }

    // A shader-internal helper is not a pipeline boundary.
    let helper = symbol(&extracted, SymbolKind::Function, "tone_map");
    assert_eq!(helper.visibility, Some(Visibility::Internal));

    symbol(&extracted, SymbolKind::Struct, "VertexOutput");
    symbol(&extracted, SymbolKind::Field, "clip_position");
    symbol(&extracted, SymbolKind::Field, "uv");

    // A module-scope binding is a declaration the host layout must match. Its
    // declared type is carried as a typed reference edge rather than only as a
    // string, so impact analysis reaches it. The `@group`/`@binding` indices are
    // deliberately not spelled into the signature: a literal-bearing signature is
    // rejected before persistence, which would blank the declared type too.
    let uniform = symbol(&extracted, SymbolKind::Variable, "view_projection");
    assert_eq!(
        uniform.signature.as_deref(),
        Some("var<uniform>: mat4x4<f32>")
    );
    symbol(&extracted, SymbolKind::Variable, "base_color_texture");
    assert!(
        extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::TypeOf && reference.name == "mat4x4<f32>"
        }),
        "the binding's declared type was not recorded as a typed edge"
    );

    // naga_oil forms the shader module graph.
    symbol(&extracted, SymbolKind::Module, "pbr::lighting");
    assert!(
        extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Imports && reference.name == "pbr::common"
        }),
        "the imported shader module was not recorded"
    );
    assert_eq!(
        extracted.parse_status,
        cartograph_domain::FileParseStatus::Parsed,
        "the shader fixture must parse cleanly"
    );

    // An intra-file call keeps callers/callees working inside one shader.
    assert!(
        extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls && reference.name == "tone_map"
        }),
        "the intra-file shader call was not recorded"
    );
}

#[test]
fn wgsl_without_declarations_is_legitimately_empty_rather_than_unsupported() {
    let extracted = extract("shaders/empty.wgsl", "// only a comment\n");
    assert_eq!(extracted.language, SourceLanguage::Wgsl);
    assert_eq!(extracted.symbols, []);
}

#[test]
fn wesl_extracts_wgsl_declarations_and_flattens_module_imports() {
    let source = r"import package::lighting::pbr;

struct Surface {
    color: vec4<f32>,
}

fn helper() {
    // import package::ignored::line_comment;
    /* import package::ignored::block_comment; */
}

public import package::lighting::shadows::sample as sample_shadow;

@fragment
fn fragment_main() -> @location(0) vec4<f32> {
    return sample_shadow();
}
";
    let extracted = extract("shaders/material.wesl", source);
    assert_eq!(extracted.language, SourceLanguage::Wesl);
    symbol(&extracted, SymbolKind::Struct, "Surface");
    let entry = symbol(&extracted, SymbolKind::Function, "fragment_main");
    assert_eq!(entry.visibility, Some(Visibility::Public));
    let imports = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Imports)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        imports,
        [
            "package::lighting::pbr",
            "package::lighting::shadows::sample"
        ]
    );
    assert!(extracted.import_bindings.iter().any(|binding| {
        binding.module_specifier == "package::lighting::shadows::sample"
            && binding.local_name == "sample_shadow"
    }));
}

#[test]
fn wesl_import_collection_is_bounded_across_multiple_statements() {
    let first = (0..300)
        .map(|index| format!("first_{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let second = (0..300)
        .map(|index| format!("second_{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let source =
        format!("import package::first::{{{first}}};\nimport package::second::{{{second}}};\n");
    let extracted = extract("shaders/bounded.wesl", &source);
    assert_eq!(
        extracted
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Imports)
            .count(),
        512
    );
    assert_eq!(extracted.import_bindings.len(), 512);
}

#[test]
fn slang_extracts_modules_imports_generics_interfaces_and_shader_entry_points() {
    let source = r#"module renderer.materials;
import renderer.common_math;

interface IShadingModel<T> {
    T shade(T value);
}

struct Surface<T> {
    T value;
};

[shader("compute")]
[numthreads(8, 8, 1)]
void computeMain(uint3 dispatchThreadId : SV_DispatchThreadID) {
    helper(dispatchThreadId);
}

void helper(uint3 dispatchThreadId) {}
"#;
    let extracted = extract("shaders/material.slang", source);
    assert_eq!(extracted.language, SourceLanguage::Slang);
    symbol(&extracted, SymbolKind::Module, "renderer.materials");
    symbol(&extracted, SymbolKind::Import, "renderer/common-math");
    assert!(
        extracted
            .symbols
            .iter()
            .any(|candidate| candidate.kind == SymbolKind::Interface
                && candidate.name.starts_with("IShadingModel")),
        "the generic Slang interface was not extracted"
    );
    assert!(
        extracted
            .symbols
            .iter()
            .any(|candidate| candidate.kind == SymbolKind::Struct
                && candidate.name.starts_with("Surface")),
        "the generic Slang struct was not extracted"
    );
    let entry = symbol(&extracted, SymbolKind::Function, "computeMain");
    assert_eq!(entry.visibility, Some(Visibility::Public));
    assert!(
        entry
            .signature
            .as_deref()
            .is_some_and(|signature| signature.starts_with("shader:compute"))
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls && reference.name == "helper"
        })
    );
}

#[test]
fn slang_parameter_blocks_and_multiple_entry_points_have_durable_spans() {
    let source = r#"module renderer.pipeline;
import renderer.camera;
import renderer.lighting;
import renderer.materials;
import renderer.geometry;
import renderer.output;

struct FrameParameters {
    float4x4 viewProjection;
};

struct MaterialParameters {
    float4 baseColor;
};

ParameterBlock<FrameParameters> frame;
ParameterBlock<MaterialParameters> material;
RWStructuredBuffer<float4> outputBuffer;

struct VertexInput {
    float3 position : POSITION;
};

struct VertexOutput {
    float4 position : SV_Position;
};

[shader("vertex")]
VertexOutput vertexMain(VertexInput input) {
    VertexOutput output;
    output.position = mul(frame.viewProjection, float4(input.position, 1.0));
    return output;
}

[shader("fragment")]
float4 fragmentMain(VertexOutput input) : SV_Target {
    return material.baseColor;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void computeMain(uint3 dispatchThreadId : SV_DispatchThreadID) {
}
"#;
    let extracted = extract("shaders/pipeline.slang", source);
    assert_eq!(extracted.language, SourceLanguage::Slang);
    symbol(&extracted, SymbolKind::Module, "renderer.pipeline");
    for entry_point in ["vertexMain", "fragmentMain", "computeMain"] {
        let entry = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.name == entry_point)
            .unwrap_or_else(|| panic!("missing Slang entry point {entry_point}"));
        assert!(entry.span.start_byte() < entry.span.end_byte());
        assert_eq!(entry.visibility, Some(Visibility::Public));
    }
}

#[test]
fn slang_missing_recovery_nodes_are_not_promoted_as_durable_symbols() {
    let source = r#"module renderer.recovery;
struct FrameParameters {
    float4x4 viewProjection;
};
ParameterBlock<FrameParameters>;

[shader("compute")]
[numthreads(8, 8, 1)]
void computeMain(uint3 dispatchThreadId : SV_DispatchThreadID) {
}
"#;
    let extracted = extract("shaders/recovery.slang", source);
    assert_eq!(
        extracted.parse_status,
        cartograph_domain::FileParseStatus::Partial
    );
    let entry = symbol(&extracted, SymbolKind::Function, "computeMain");
    assert!(entry.span.start_byte() < entry.span.end_byte());
}

#[test]
fn slang_keyword_named_resources_do_not_promote_missing_declarators() {
    let source = r#"RWStructuredBuffer<uint> out;

[shader("compute")]
void computeMain(uint3 dispatchID : SV_DispatchThreadID) {
    out[0] = dispatchID.x;
}
"#;
    let extracted = extract("shaders/keyword-resource.slang", source);
    let entry = symbol(&extracted, SymbolKind::Function, "computeMain");
    assert!(entry.span.start_byte() < entry.span.end_byte());
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !symbol.name.is_empty()
                && symbol.span.start_byte() < symbol.span.end_byte())
    );
}

#[test]
fn metal_reuses_the_c_family_slice_for_kernels_and_structs() {
    let source = r"#include <metal_stdlib>
using namespace metal;

struct Uniforms {
    float4x4 modelViewProjection;
};

float4 tonemap(float4 color) {
    return color;
}

kernel void compute_main(device float4 *output [[buffer(0)]],
                         uint index [[thread_position_in_grid]]) {
    output[index] = tonemap(output[index]);
}
";
    let extracted = extract("shaders/pipeline.metal", source);
    assert_eq!(extracted.language, SourceLanguage::Metal);
    symbol(&extracted, SymbolKind::Struct, "Uniforms");
    assert!(
        extracted
            .symbols
            .iter()
            .any(|symbol| symbol.name == "tonemap"),
        "the Metal helper function was not extracted"
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls && reference.name == "tonemap"
        }),
        "the Metal call was not recorded"
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    let snapshot =
        SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new_for_capability_validation(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| {
            let available = extracted
                .symbols
                .iter()
                .map(|symbol| format!("{:?} {}", symbol.kind, symbol.name))
                .collect::<Vec<_>>();
            panic!("missing {kind:?} {name}; extracted: {available:?}")
        })
}

#[test]
fn quoted_path_imports_stay_explicitly_unparsed_rather_than_silently_dropped() {
    // The pinned grammar accepts naga_oil module-path imports but not the
    // quoted-file form. That gap must surface as a recoverable diagnostic, never
    // as a file that looks successfully empty.
    let extracted = extract("shaders/quoted.wgsl", "#import \"shaders/common.wgsl\"\n");
    assert_eq!(
        extracted.parse_status,
        cartograph_domain::FileParseStatus::Partial,
        "an unparsed import form must be reported, not silently skipped"
    );
    assert!(
        !extracted.diagnostics.is_empty(),
        "a partial shader parse must carry a diagnostic"
    );
}
