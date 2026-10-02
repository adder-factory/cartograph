use std::collections::{BTreeSet, HashMap};

use cartograph_domain::{
    FileParseStatus, ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility,
    declaration_value_is_search_safe,
};
use tree_sitter::Node;

use crate::{
    Containment, DiagnosticCode, ExtractError, ExtractedFile, ExtractedImportBinding,
    ExtractedReference, ExtractedSymbol, ExtractionDiagnostic, ExtractionStrategy, LanguageSpec,
    SourceSnapshot, SymbolExecutionFlags, SymbolExportFlags, SymbolImplementationFlags,
    budget::{
        ExtractionBudget, containment_budget_bytes, diagnostic_budget_bytes,
        import_binding_budget_bytes, reference_budget_bytes, symbol_budget_bytes,
    },
    identity::SymbolIdentity,
};

mod ada_family;
mod c_family;
mod declarations;
mod dynamic_dispatch;
mod embedded_sql;
mod generic_family;
mod graphql_family;
mod jvm_dynamic_family;
mod managed_family;
mod module_system;
mod numerical;
mod polyglot;
mod prisma_family;
mod references;
mod rust_macro;
mod schema;
mod shader_family;
mod shell_family;
mod sql_family;
pub(crate) mod syntax;
mod value_references;

use syntax::{
    body_search_text, callable_signature, clone_token_profile, collect_diagnostics, contains_jsx,
    export_flags, has_child_kind, jsdoc, named_children, span_for, starts_uppercase,
    structural_digest, visibility,
};

const MAX_AST_DEPTH: usize = crate::MAXIMUM_AST_DEPTH;
const MAX_BOUNDED_AST_VISITS: usize = 500_000;
const AST_VISIT_CANCELLATION_INTERVAL: usize = 256;
const MAX_SAFE_SIGNATURE_BYTES: usize = 512;

#[derive(Default)]
struct AstVisitBudget<const MAXIMUM_DEPTH: usize> {
    visits: usize,
}

impl<const MAXIMUM_DEPTH: usize> AstVisitBudget<MAXIMUM_DEPTH> {
    fn observe(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        depth: usize,
    ) -> Result<(), ExtractError> {
        if depth > MAXIMUM_DEPTH.min(builder.maximum_ast_depth) {
            return Err(ExtractError::NestingLimit);
        }
        self.visits = self
            .visits
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.visits > MAX_BOUNDED_AST_VISITS {
            return Err(ExtractError::OutputLimit);
        }
        if self.visits.is_multiple_of(AST_VISIT_CANCELLATION_INTERVAL) {
            builder.context.ensure_active()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WalkInput<'tree> {
    root: Node<'tree>,
    parse_status: FileParseStatus,
    maximum_ast_depth: usize,
}

impl<'tree> WalkInput<'tree> {
    pub(crate) const fn new(
        root: Node<'tree>,
        parse_status: FileParseStatus,
        maximum_ast_depth: usize,
    ) -> Self {
        Self {
            root,
            parse_status,
            maximum_ast_depth,
        }
    }
}

pub(crate) fn extract(
    snapshot: &SourceSnapshot,
    input: WalkInput<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ExtractedFile, ExtractError> {
    let strategy = LanguageSpec::for_language(snapshot.language()).strategy();
    if !strategy.is_executable() {
        return Err(ExtractError::UnsupportedLanguage);
    }
    let mut builder = ExtractionBuilder::new(snapshot, input.maximum_ast_depth, cancelled)?;
    if strategy != ExtractionStrategy::ParserOnly {
        enrich_extraction(&mut builder, input.root)?;
    }
    let diagnostics = collect_extraction_diagnostics(&mut builder, input)?;
    finish_extraction(builder, input, diagnostics)
}

fn enrich_extraction(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    module_system::collect_explicit_exports(builder, root)?;
    shader_family::collect_wesl_imports(builder)?;
    builder.visit(root, 0)?;
    numerical::enrich(builder, root)?;
    dynamic_dispatch::enrich(builder, root)?;
    schema::enrich(builder, root)?;
    embedded_sql::enrich(builder, root)?;
    value_references::enrich(builder, root)
}

fn collect_extraction_diagnostics(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: WalkInput<'_>,
) -> Result<Vec<ExtractionDiagnostic>, ExtractError> {
    let mut diagnostics = if input.parse_status == FileParseStatus::Partial {
        let diagnostics = collect_diagnostics(input.root, builder.context.cancelled)?;
        if diagnostics.is_empty() {
            vec![ExtractionDiagnostic {
                code: DiagnosticCode::SyntaxError,
                span: None,
            }]
        } else {
            diagnostics
        }
    } else {
        Vec::new()
    };
    // A shortened name keeps the generation publishable, but the file no longer
    // carries the exact synthesized identity and must say so.
    if builder.shortened_canonical_names {
        diagnostics.push(ExtractionDiagnostic {
            code: DiagnosticCode::CanonicalNameTruncated,
            span: None,
        });
    }
    for _ in &diagnostics {
        builder
            .context
            .budget
            .reserve_fact(diagnostic_budget_bytes(), std::iter::empty())?;
    }
    Ok(diagnostics)
}

fn finish_extraction(
    mut builder: ExtractionBuilder<'_, '_>,
    input: WalkInput<'_>,
    diagnostics: Vec<ExtractionDiagnostic>,
) -> Result<ExtractedFile, ExtractError> {
    let snapshot = builder.context.snapshot;
    let output_limit = builder.context.budget.output_limit();
    let has_inline_tests = has_inline_tests(&mut builder, input.root)?;
    let file = ExtractedFile {
        file_id: snapshot.file_id().clone(),
        path: snapshot.path().clone(),
        language: snapshot.language(),
        content_hash: snapshot.content_hash().clone(),
        byte_size: snapshot.byte_size(),
        line_count: snapshot.line_count(),
        parse_status: input.parse_status,
        symbols: builder.facts.symbols,
        containments: builder.facts.containments,
        references: builder.facts.references,
        numerical_sites: builder.facts.numerical_sites,
        import_bindings: builder.facts.import_bindings,
        has_inline_tests,
        test_search_text: String::new(),
        test_search_truncated: false,
        diagnostics,
    };
    if file.modeled_retained_bytes() > output_limit {
        return Err(ExtractError::OutputLimit);
    }
    Ok(file)
}

fn has_inline_tests(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<bool, ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Rust {
        return Ok(false);
    }
    for node in syntax::descendants_including_root(root) {
        if (builder.context.cancelled)() {
            return Err(ExtractError::Cancelled);
        }
        if node.kind() != "attribute_item" || !rust_attribute_precedes_function(node) {
            continue;
        }
        let compact = compact_rust_attribute(builder.context.source(), node)?;
        if rust_attribute_marks_test(&compact) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn compact_rust_attribute(source: &str, node: Node<'_>) -> Result<String, ExtractError> {
    let attribute = source
        .get(node.start_byte()..node.end_byte())
        .ok_or(ExtractError::InvalidSpan)?;
    let mut compact = String::new();
    compact
        .try_reserve(attribute.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    compact.extend(
        attribute
            .chars()
            .filter(|character| !character.is_whitespace()),
    );
    Ok(compact)
}

fn rust_attribute_marks_test(compact: &str) -> bool {
    matches!(compact, "#[test]" | "#[rstest]")
        || compact.starts_with("#[tokio::test")
        || compact.starts_with("#[async_std::test")
        || compact.starts_with("#[actix_rt::test")
        || compact.starts_with("#[test_case")
        || compact.starts_with("#[rstest(")
        || compact.starts_with("#[cfg(") && compact.contains("test")
}

fn rust_symbol_is_test_owned(
    language: SourceLanguage,
    node: Node<'_>,
    source: &str,
) -> Result<bool, ExtractError> {
    if language != SourceLanguage::Rust {
        return Ok(false);
    }
    let mut current = Some(node);
    for _ in 0..=MAX_AST_DEPTH {
        let Some(candidate) = current else {
            return Ok(false);
        };
        if candidate.kind() == "mod_item"
            && candidate
                .child_by_field_name("name")
                .and_then(|name| source.get(name.start_byte()..name.end_byte()))
                .is_some_and(rust_test_module_name)
        {
            return Ok(true);
        }
        let mut sibling = candidate.prev_named_sibling();
        for _ in 0..16 {
            let Some(attribute) = sibling else {
                break;
            };
            if attribute.kind() != "attribute_item" {
                break;
            }
            if rust_attribute_marks_test(&compact_rust_attribute(source, attribute)?) {
                return Ok(true);
            }
            sibling = attribute.prev_named_sibling();
        }
        current = candidate.parent();
    }
    Err(ExtractError::NestingLimit)
}

fn rust_test_module_name(name: &str) -> bool {
    matches!(name, "test" | "tests" | "test_support" | "contract_tests")
        || name.starts_with("test_")
        || name.ends_with("_tests")
}

fn rust_attribute_precedes_function(node: Node<'_>) -> bool {
    let mut sibling = node.next_named_sibling();
    for _ in 0..16 {
        let Some(candidate) = sibling else {
            return false;
        };
        match candidate.kind() {
            "function_item" => return true,
            "attribute_item" => sibling = candidate.next_named_sibling(),
            _ => return false,
        }
    }
    false
}

struct ExtractionBuilder<'source, 'cancel> {
    context: ExtractionContext<'source, 'cancel>,
    identities: SymbolIdentity<'source>,
    facts: ExtractionFacts,
    owners: Vec<SymbolId>,
    native_owner_kinds: Vec<SymbolKind>,
    native_visibilities: Vec<Option<Visibility>>,
    native_scope_symbols: HashMap<String, Option<(SymbolId, SymbolKind)>>,
    qualifiers: Vec<String>,
    explicit_exports: BTreeSet<String>,
    explicit_default_exports: BTreeSet<String>,
    commonjs_shadowing: module_system::CommonJsShadowing,
    maximum_ast_depth: usize,
    /// Whether any synthesized name exceeded its canonical bound and had to be
    /// deterministically shortened for this file.
    shortened_canonical_names: bool,
}

fn visit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let (kind, body_kind) = match node.kind() {
        "interface_declaration" => (SymbolKind::Interface, "interface_body"),
        "class_declaration" | "abstract_class_declaration" => (SymbolKind::Class, "class_body"),
        _ => return builder.visit_named_children(node, depth),
    };
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let (exported, default_export) = export_flags(node);
    let pending = PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::new(exported, default_export),
        async_symbol: false,
        static_member: false,
        visibility: visibility(node, builder.context.source()),
    };
    let id = builder.emit_symbol(pending)?;
    builder.owners.push(id.clone());
    builder.qualifiers.push(name);
    references::capture_heritage(builder, node, &id)?;
    for child in named_children(node) {
        if child.kind() == body_kind {
            builder.visit(child, depth.saturating_add(1))?;
        }
    }

    builder.qualifiers.pop();
    builder.owners.pop();
    Ok(())
}

fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let declared_kind = if matches!(
        node.kind(),
        "method_definition" | "method_signature" | "abstract_method_signature"
    ) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let component = declared_kind == SymbolKind::Function
        && starts_uppercase(&name)
        && contains_jsx(node, builder.context.cancelled)?;
    let kind = if component {
        SymbolKind::Component
    } else {
        declared_kind
    };
    let (exported, default_export) = export_flags(node);
    let body = node.child_by_field_name("body");
    let pending = PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: body.is_none(),
        signature: builder.context.callable_signature(node)?,
        export: SymbolExportFlags::new(exported, default_export),
        async_symbol: has_child_kind(node, "async"),
        static_member: has_child_kind(node, "static"),
        visibility: visibility(node, builder.context.source()),
    };
    let id = builder.emit_symbol(pending)?;
    references::capture_callable_types(builder, node, &id)?;
    builder.owners.push(id);
    builder.qualifiers.push(name);
    emit_javascript_callable_parameters(builder, node)?;
    if let Some(body) = body {
        builder.visit(body, depth.saturating_add(1))?;
    }
    builder.qualifiers.pop();
    builder.owners.pop();
    Ok(())
}

fn visit_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let constant = has_child_kind(declaration, "const");
    for declarator in
        named_children(declaration).filter(|node| node.kind() == "variable_declarator")
    {
        visit_javascript_binding(
            builder,
            JavaScriptBindingVisit {
                declaration,
                declarator,
                depth,
                constant,
            },
        )?;
    }
    Ok(())
}

fn owner_for_node(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<SymbolId> {
    let start = u64::try_from(node.start_byte()).ok()?;
    let end = u64::try_from(node.end_byte()).ok()?;
    builder
        .facts
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.span.start_byte() <= start
                && end <= symbol.span.end_byte()
                && !matches!(symbol.kind, SymbolKind::File | SymbolKind::Import)
        })
        .min_by_key(|symbol| {
            symbol
                .span
                .end_byte()
                .saturating_sub(symbol.span.start_byte())
        })
        .map(|symbol| symbol.id.clone())
}

struct ExtractionContext<'source, 'cancel> {
    snapshot: &'source SourceSnapshot,
    cancelled: &'cancel mut dyn FnMut() -> bool,
    budget: ExtractionBudget,
}

#[derive(Default)]
struct ExtractionFacts {
    symbols: Vec<ExtractedSymbol>,
    containments: Vec<Containment>,
    references: Vec<ExtractedReference>,
    numerical_sites: Vec<crate::ExtractedNumericalSite>,
    import_bindings: Vec<ExtractedImportBinding>,
}

struct PendingSymbol<'tree> {
    kind: SymbolKind,
    name: String,
    span_node: Node<'tree>,
    structural_node: Node<'tree>,
    doc_anchor: Node<'tree>,
    body_node: Option<Node<'tree>>,
    declaration_only: bool,
    signature: Option<String>,
    export: SymbolExportFlags,
    async_symbol: bool,
    static_member: bool,
    visibility: Option<Visibility>,
}

impl<'tree> PendingSymbol<'tree> {
    fn namespace(node: Node<'tree>, name: String) -> Self {
        Self {
            kind: SymbolKind::Namespace,
            name,
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: SymbolExportFlags::named(true),
            async_symbol: false,
            static_member: false,
            visibility: None,
        }
    }
}

fn current_owner_kind_in(builder: &ExtractionBuilder<'_, '_>, allowed: &[SymbolKind]) -> bool {
    builder
        .native_owner_kinds
        .last()
        .is_some_and(|kind| allowed.contains(kind))
}

fn safe_assignment_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    value: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.text(value).trim();
    let signature_length = raw.len().checked_add(2).ok_or(ExtractError::OutputLimit)?;
    if signature_length > MAX_SAFE_SIGNATURE_BYTES || !declaration_value_is_search_safe(raw) {
        return Ok(None);
    }
    builder
        .context
        .budget
        .ensure_string_length(signature_length)?;
    let mut signature = String::new();
    signature
        .try_reserve(signature_length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str("= ");
    signature.push_str(raw);
    Ok(Some(signature))
}

#[derive(Clone, Copy)]
struct JoinedSignature<'text> {
    left: &'text str,
    separator: char,
    right: &'text str,
}

impl<'text> JoinedSignature<'text> {
    const fn words(left: &'text str, right: &'text str) -> Self {
        Self {
            left,
            separator: ' ',
            right,
        }
    }

    const fn dotted(left: &'text str, right: &'text str) -> Self {
        Self {
            left,
            separator: '.',
            right,
        }
    }
}

fn joined_signature(
    builder: &ExtractionBuilder<'_, '_>,
    input: JoinedSignature<'_>,
) -> Result<String, ExtractError> {
    let length = input
        .left
        .len()
        .checked_add(input.right.len())
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(input.left);
    signature.push(input.separator);
    signature.push_str(input.right);
    builder.context.copy_text(&signature)
}

#[derive(Clone, Copy)]
struct SingleChildUnwrap {
    terminal: for<'tree> fn(Node<'tree>) -> bool,
    wrappers: &'static [&'static str],
}

impl SingleChildUnwrap {
    const fn new(
        terminal: for<'tree> fn(Node<'tree>) -> bool,
        wrappers: &'static [&'static str],
    ) -> Self {
        Self { terminal, wrappers }
    }
}

fn unwrap_single_child(node: Node<'_>, depth: usize, rule: SingleChildUnwrap) -> Option<Node<'_>> {
    if depth > 8 {
        return None;
    }
    if (rule.terminal)(node) {
        return Some(node);
    }
    if rule.wrappers.contains(&node.kind()) {
        let mut children = named_children(node);
        let child = children.next()?;
        if children.next().is_none() {
            return unwrap_single_child(child, depth.saturating_add(1), rule);
        }
    }
    None
}

fn with_root_scope<Output>(
    builder: &mut ExtractionBuilder<'_, '_>,
    operation: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Output,
) -> Output {
    let owners = std::mem::take(&mut builder.owners);
    let owner_kinds = std::mem::take(&mut builder.native_owner_kinds);
    let visibilities = std::mem::take(&mut builder.native_visibilities);
    let qualifiers = std::mem::take(&mut builder.qualifiers);
    let output = operation(builder);
    builder.owners = owners;
    builder.native_owner_kinds = owner_kinds;
    builder.native_visibilities = visibilities;
    builder.qualifiers = qualifiers;
    output
}

#[derive(Clone, Copy)]
enum ChildReferenceKind {
    CCall,
    ManagedConstruction,
}

fn capture_child_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: ChildReferenceKind,
) -> Result<(), ExtractError> {
    let (field, reference_kind) = match kind {
        ChildReferenceKind::CCall => ("function", ReferenceKind::Calls),
        ChildReferenceKind::ManagedConstruction => ("type", ReferenceKind::Instantiates),
    };
    let Some(target) = node.child_by_field_name(field) else {
        return Ok(());
    };
    let name = match kind {
        ChildReferenceKind::CCall => c_family::safe_call_target(builder, target)?,
        ChildReferenceKind::ManagedConstruction => {
            managed_family::managed_outer_type_name(builder, target)?
        }
    };
    let Some(name) = name else {
        return Ok(());
    };
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: reference_kind,
            node: target,
        },
    )
}

pub(super) struct PendingReference<'tree> {
    owner: Option<SymbolId>,
    name: String,
    kind: ReferenceKind,
    node: Node<'tree>,
}

#[derive(Clone, Copy)]
enum ExtractionFamily {
    Ada,
    Shader,
    Shell,
    C,
    Managed,
    JvmDynamic,
    GraphQl,
    Prisma,
    Sql,
    Generic,
    Polyglot,
    JavaScript,
    Unsupported,
}

const SHELL_LANGUAGES: &[SourceLanguage] = &[
    SourceLanguage::Bash,
    SourceLanguage::Fish,
    SourceLanguage::PowerShell,
    SourceLanguage::Zsh,
];
const ADA_LANGUAGES: &[SourceLanguage] = &[SourceLanguage::Ada, SourceLanguage::Vhdl];
const C_LANGUAGES: &[SourceLanguage] = &[
    SourceLanguage::C,
    SourceLanguage::Cpp,
    SourceLanguage::Cuda,
    SourceLanguage::Glsl,
    SourceLanguage::Hlsl,
    SourceLanguage::Metal,
    SourceLanguage::Slang,
];
const MANAGED_LANGUAGES: &[SourceLanguage] = &[SourceLanguage::Java, SourceLanguage::CSharp];
const JVM_DYNAMIC_LANGUAGES: &[SourceLanguage] = &[
    SourceLanguage::Kotlin,
    SourceLanguage::Scala,
    SourceLanguage::Groovy,
];
const GENERIC_LANGUAGES: &[SourceLanguage] = &[
    SourceLanguage::Abap,
    SourceLanguage::Apex,
    SourceLanguage::ArkTs,
    SourceLanguage::Astro,
    SourceLanguage::Clojure,
    SourceLanguage::CommonLisp,
    SourceLanguage::Dart,
    SourceLanguage::FSharp,
    SourceLanguage::Hcl,
    SourceLanguage::Html,
    SourceLanguage::Khn,
    SourceLanguage::Lean,
    SourceLanguage::Lua,
    SourceLanguage::Luau,
    SourceLanguage::Nix,
    SourceLanguage::ObjectiveC,
    SourceLanguage::Pascal,
    SourceLanguage::Php,
    SourceLanguage::R,
    SourceLanguage::ReScript,
    SourceLanguage::Ruby,
    SourceLanguage::Solidity,
    SourceLanguage::Swift,
    SourceLanguage::VbNet,
    SourceLanguage::Yaml,
];
const POLYGLOT_LANGUAGES: &[SourceLanguage] = &[
    SourceLanguage::Rust,
    SourceLanguage::Python,
    SourceLanguage::Go,
];
const JAVASCRIPT_LANGUAGES: &[SourceLanguage] = &[
    SourceLanguage::TypeScript,
    SourceLanguage::Tsx,
    SourceLanguage::JavaScript,
    SourceLanguage::Jsx,
];

/// Declaration and usage entry points for one extraction family.
///
/// Pairing them keeps a family from being wired into one traversal and
/// forgotten in the other, which would make its files look successfully empty.
struct FamilySlice {
    visit_declaration:
        fn(&mut ExtractionBuilder<'_, '_>, Node<'_>, usize) -> Result<bool, ExtractError>,
    visit_usage: fn(&mut ExtractionBuilder<'_, '_>, Node<'_>, usize) -> Result<(), ExtractError>,
}

/// Structural family slices for the walker-driven language families.
const fn family_slice_structural(family: ExtractionFamily) -> Option<FamilySlice> {
    let slice = match family {
        ExtractionFamily::Ada => FamilySlice {
            visit_declaration: ada_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                ada_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::Shader => FamilySlice {
            visit_declaration: shader_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                shader_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::Shell => FamilySlice {
            visit_declaration: shell_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                shell_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::C => FamilySlice {
            visit_declaration: c_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                c_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::Managed => FamilySlice {
            visit_declaration: managed_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                managed_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::JvmDynamic => FamilySlice {
            visit_declaration: jvm_dynamic_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                jvm_dynamic_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        _ => return None,
    };
    Some(slice)
}

/// Slices for the declarative, generic, and JavaScript-family languages.
const fn family_slice_declarative(family: ExtractionFamily) -> Option<FamilySlice> {
    let slice = match family {
        ExtractionFamily::GraphQl => FamilySlice {
            visit_declaration: graphql_family::visit_declaration,
            visit_usage: |builder, node, depth| builder.visit_named_children(node, depth),
        },
        ExtractionFamily::Prisma => FamilySlice {
            visit_declaration: prisma_family::visit_declaration,
            visit_usage: |builder, node, depth| builder.visit_named_children(node, depth),
        },
        ExtractionFamily::Sql => FamilySlice {
            visit_declaration: sql_family::visit_declaration,
            visit_usage: |builder, node, depth| builder.visit_named_children(node, depth),
        },
        ExtractionFamily::Generic => FamilySlice {
            visit_declaration: generic_family::visit_declaration,
            visit_usage: |builder, node, depth| {
                generic_family::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::Polyglot => FamilySlice {
            visit_declaration: polyglot::visit_declaration,
            visit_usage: |builder, node, depth| {
                polyglot::capture_usage(builder, node)?;
                builder.visit_named_children(node, depth)
            },
        },
        ExtractionFamily::JavaScript => FamilySlice {
            visit_declaration: visit_javascript_declaration,
            visit_usage: visit_javascript_usage,
        },
        _ => return None,
    };
    Some(slice)
}

/// Entry points for one extraction family, or `None` for unsupported source.
///
/// The families are split across two lookups for the same reason the grammar
/// bindings are: one exhaustive match over every family would make this the
/// single highest-fan-out symbol in the crate.
const fn family_slice(family: ExtractionFamily) -> Option<FamilySlice> {
    if let Some(slice) = family_slice_structural(family) {
        return Some(slice);
    }
    family_slice_declarative(family)
}

fn extraction_family(language: SourceLanguage) -> ExtractionFamily {
    if ADA_LANGUAGES.contains(&language) {
        ExtractionFamily::Ada
    } else if SHELL_LANGUAGES.contains(&language) {
        ExtractionFamily::Shell
    } else if C_LANGUAGES.contains(&language) {
        ExtractionFamily::C
    } else if MANAGED_LANGUAGES.contains(&language) {
        ExtractionFamily::Managed
    } else if JVM_DYNAMIC_LANGUAGES.contains(&language) {
        ExtractionFamily::JvmDynamic
    } else if GENERIC_LANGUAGES.contains(&language) {
        ExtractionFamily::Generic
    } else if POLYGLOT_LANGUAGES.contains(&language) {
        ExtractionFamily::Polyglot
    } else if JAVASCRIPT_LANGUAGES.contains(&language) {
        ExtractionFamily::JavaScript
    } else {
        match language {
            SourceLanguage::GraphQl => ExtractionFamily::GraphQl,
            SourceLanguage::Wesl | SourceLanguage::Wgsl => ExtractionFamily::Shader,
            SourceLanguage::Prisma => ExtractionFamily::Prisma,
            SourceLanguage::Sql => ExtractionFamily::Sql,
            _ => ExtractionFamily::Unsupported,
        }
    }
}

fn visit_javascript_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "interface_declaration" | "class_declaration" | "abstract_class_declaration" => {
            visit_container(builder, node, depth)?;
        }
        "function_declaration"
        | "generator_function_declaration"
        | "function_signature"
        | "method_definition"
        | "method_signature"
        | "abstract_method_signature" => visit_callable(builder, node, depth)?,
        "lexical_declaration" | "variable_declaration" => visit_bindings(builder, node, depth)?,
        "import_statement" => declarations::visit_import(builder, node)?,
        "export_statement" => declarations::visit_export(builder, node, depth)?,
        "type_alias_declaration" => declarations::visit_type_alias(builder, node, depth)?,
        "enum_declaration" => declarations::visit_enum(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_javascript_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    module_system::capture_commonjs_assignment(builder, node)?;
    match node.kind() {
        "call_expression" => {
            if !module_system::capture_dynamic_import(builder, node)? {
                references::capture_invocation(builder, node, references::InvocationKind::Call)?;
            }
        }
        "new_expression" => {
            references::capture_invocation(
                builder,
                node,
                references::InvocationKind::Construction,
            )?;
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            references::capture_jsx_reference(builder, node)?;
        }
        "member_expression" => references::capture_field_access(builder, node)?,
        _ => {}
    }
    builder.visit_named_children(node, depth)
}

#[derive(Clone, Copy)]
struct JavaScriptBindingVisit<'tree> {
    declaration: Node<'tree>,
    declarator: Node<'tree>,
    depth: usize,
    constant: bool,
}

#[derive(Clone, Copy)]
struct JavaScriptBindingValue<'tree, 'context> {
    value: Node<'tree>,
    callable: Option<Node<'tree>>,
    id: &'context SymbolId,
    name: &'context str,
    depth: usize,
}

#[derive(Clone, Copy)]
struct DestructuredBindingVisit<'tree> {
    binding: JavaScriptBindingVisit<'tree>,
    name_node: Node<'tree>,
    value: Option<Node<'tree>>,
    static_module_binding: bool,
}

fn javascript_binding_kind(component: bool, callable: bool, constant: bool) -> SymbolKind {
    if component {
        SymbolKind::Component
    } else if callable {
        SymbolKind::Function
    } else if constant {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    }
}

fn javascript_binding_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    callable: Option<Node<'_>>,
    value: Option<Node<'_>>,
) -> Result<Option<String>, ExtractError> {
    if let Some(callable_node) = callable {
        builder.context.callable_signature(callable_node)
    } else if let Some(value_node) = value {
        builder.context.assignment_signature(value_node)
    } else {
        Ok(None)
    }
}

fn visit_destructured_javascript_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: DestructuredBindingVisit<'_>,
) -> Result<(), ExtractError> {
    if input.static_module_binding {
        return Ok(());
    }
    let kind = if input.binding.constant {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    emit_javascript_binding_tree(builder, input.name_node, kind)?;
    if let Some(value_node) = input.value {
        builder.visit(value_node, input.binding.depth.saturating_add(1))?;
    }
    Ok(())
}

fn visit_javascript_binding_value(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: JavaScriptBindingValue<'_, '_>,
) -> Result<(), ExtractError> {
    builder.owners.push(input.id.clone());
    builder.qualifiers.push(input.name.to_owned());
    if let Some(callable_node) = input.callable {
        emit_javascript_callable_parameters(builder, callable_node)?;
    }
    let visit_node = input
        .callable
        .and_then(|callable_node| callable_node.child_by_field_name("body"))
        .unwrap_or(input.value);
    builder.visit(visit_node, input.depth.saturating_add(1))?;
    builder.qualifiers.pop();
    builder.owners.pop();
    Ok(())
}

fn visit_javascript_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: JavaScriptBindingVisit<'_>,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    let Some(name_node) = input.declarator.child_by_field_name("name") else {
        return Ok(());
    };
    let value = input.declarator.child_by_field_name("value");
    let static_module_binding = module_system::is_static_module_binding_value(builder, value);
    module_system::capture_commonjs_require(builder, name_node, value)?;
    module_system::capture_dynamic_import_binding(builder, name_node, value)?;
    if !matches!(name_node.kind(), "identifier" | "property_identifier") {
        return visit_destructured_javascript_binding(
            builder,
            DestructuredBindingVisit {
                binding: input,
                name_node,
                value,
                static_module_binding,
            },
        );
    }
    let name = builder.context.owned_text(name_node)?;
    let callable =
        value.filter(|value| matches!(value.kind(), "arrow_function" | "function_expression"));
    let symbol_node = callable.unwrap_or(input.declarator);
    let component = callable.is_some()
        && starts_uppercase(&name)
        && contains_jsx(symbol_node, builder.context.cancelled)?;
    let (exported, default_export) = export_flags(symbol_node);
    let pending = PendingSymbol {
        kind: javascript_binding_kind(component, callable.is_some(), input.constant),
        name: name.clone(),
        span_node: symbol_node,
        structural_node: symbol_node,
        doc_anchor: input.declaration,
        body_node: value,
        declaration_only: false,
        signature: javascript_binding_signature(builder, callable, value)?,
        export: SymbolExportFlags::new(exported, default_export),
        async_symbol: callable.is_some_and(|entry| has_child_kind(entry, "async")),
        static_member: false,
        visibility: None,
    };
    let id = builder.emit_symbol(pending)?;
    if callable.is_some() {
        references::capture_callable_types(builder, symbol_node, &id)?;
    } else {
        references::capture_type_nodes(builder, input.declarator, &id)?;
    }
    if let Some(value) = value {
        visit_javascript_binding_value(
            builder,
            JavaScriptBindingValue {
                value,
                callable,
                id: &id,
                name: &name,
                depth: input.depth,
            },
        )?;
    }
    Ok(())
}

impl<'source, 'cancel> ExtractionBuilder<'source, 'cancel> {
    fn new(
        snapshot: &'source SourceSnapshot,
        maximum_ast_depth: usize,
        cancelled: &'cancel mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        Ok(Self {
            context: ExtractionContext::new(snapshot, cancelled)?,
            identities: SymbolIdentity::new(snapshot.path()),
            facts: ExtractionFacts::default(),
            owners: Vec::new(),
            native_owner_kinds: Vec::new(),
            native_visibilities: Vec::new(),
            native_scope_symbols: HashMap::new(),
            qualifiers: Vec::new(),
            explicit_exports: BTreeSet::new(),
            explicit_default_exports: BTreeSet::new(),
            commonjs_shadowing: module_system::CommonJsShadowing::default(),
            maximum_ast_depth,
            shortened_canonical_names: false,
        })
    }

    fn visit(&mut self, node: Node<'_>, depth: usize) -> Result<(), ExtractError> {
        self.context.ensure_active()?;
        if depth > self.maximum_ast_depth {
            return Err(ExtractError::NestingLimit);
        }
        if self.visit_declaration(node, depth)? {
            return Ok(());
        }
        self.visit_usage(node, depth)
    }

    fn visit_declaration(&mut self, node: Node<'_>, depth: usize) -> Result<bool, ExtractError> {
        let family = extraction_family(self.context.snapshot.language());
        let Some(slice) = family_slice(family) else {
            return Err(ExtractError::UnsupportedLanguage);
        };
        (slice.visit_declaration)(self, node, depth)
    }

    fn visit_usage(&mut self, node: Node<'_>, depth: usize) -> Result<(), ExtractError> {
        let family = extraction_family(self.context.snapshot.language());
        let Some(slice) = family_slice(family) else {
            return Err(ExtractError::UnsupportedLanguage);
        };
        (slice.visit_usage)(self, node, depth)
    }

    fn visit_named_children(&mut self, node: Node<'_>, depth: usize) -> Result<(), ExtractError> {
        for child in named_children(node) {
            self.visit(child, depth.saturating_add(1))?;
        }
        Ok(())
    }

    fn emit_symbol(&mut self, pending: PendingSymbol<'_>) -> Result<SymbolId, ExtractError> {
        self.context.ensure_active()?;
        let qualified_name = self.qualified_name(&pending.name)?;
        let qualified_name = self.bound_canonical_name(
            qualified_name,
            crate::bounded_name::MAX_CANONICAL_QUALIFIED_NAME_BYTES,
        );
        let id = self.identities.next(pending.kind, &qualified_name)?;
        self.reserve_parent_containment(&id)?;
        let symbol = self.extracted_symbol(pending, &id, qualified_name)?;
        self.context.budget.reserve_fact(
            symbol_budget_bytes(&symbol),
            [
                symbol.id.as_str(),
                symbol.name.as_str(),
                symbol.qualified_name.as_str(),
                symbol.signature.as_deref().unwrap_or_default(),
                symbol.docstring.as_deref().unwrap_or_default(),
                symbol.body_search_text.as_str(),
                symbol.structural_digest.as_str(),
                symbol.clone_shape_digest.as_str(),
            ],
        )?;
        self.facts.symbols.push(symbol);
        Ok(id)
    }

    fn reserve_parent_containment(&mut self, id: &SymbolId) -> Result<(), ExtractError> {
        if let Some(parent) = self.owners.last() {
            let containment = Containment {
                parent: parent.clone(),
                child: id.clone(),
            };
            self.context.budget.reserve_fact(
                containment_budget_bytes(&containment),
                [containment.parent.as_str(), containment.child.as_str()],
            )?;
            self.facts.containments.push(containment);
        }
        Ok(())
    }

    fn extracted_symbol(
        &mut self,
        pending: PendingSymbol<'_>,
        id: &SymbolId,
        qualified_name: String,
    ) -> Result<ExtractedSymbol, ExtractError> {
        let span = span_for(pending.span_node)?;
        let docstring = self.context.jsdoc(pending.doc_anchor)?;
        let body_search = match pending.body_node {
            Some(body) => {
                body_search_text(body, self.context.snapshot.source(), self.context.cancelled)?
            }
            None => syntax::BodySearchText::default(),
        };
        let health = syntax::symbol_health_metrics(
            syntax::SymbolHealthInput {
                declaration: pending.structural_node,
                body: pending.body_node,
                symbol_kind: pending.kind,
                symbol_name: &pending.name,
                signature: pending.signature.as_deref(),
                docstring: docstring.as_deref(),
                language: self.context.snapshot.language(),
                async_symbol: pending.async_symbol,
                source: self.context.snapshot.source(),
            },
            self.context.cancelled,
        )?;
        let structural_digest = structural_digest(
            pending.structural_node,
            self.context.snapshot.source(),
            self.context.cancelled,
        )?;
        let clone_shape_digest =
            syntax::clone_shape_digest(pending.structural_node, self.context.cancelled)?;
        let clone_token_profile = matches!(
            pending.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
        )
        .then(|| {
            clone_token_profile(
                pending.structural_node,
                self.context.snapshot.source(),
                self.context.cancelled,
            )
        })
        .transpose()?
        .flatten();
        let top_level = self.owners.is_empty();
        let explicit_export = top_level && self.explicit_exports.contains(&pending.name);
        let explicit_default = top_level && self.explicit_default_exports.contains(&pending.name);
        Ok(ExtractedSymbol {
            id: id.clone(),
            kind: pending.kind,
            name: pending.name,
            qualified_name,
            span,
            signature: pending.signature,
            docstring,
            body_search_text: body_search.text,
            body_search_truncated: body_search.truncated,
            health,
            implementation: SymbolImplementationFlags {
                declaration_only: pending.declaration_only,
                test_symbol: rust_symbol_is_test_owned(
                    self.context.snapshot.language(),
                    pending.structural_node,
                    self.context.snapshot.source(),
                )?,
            },
            export: SymbolExportFlags::new(
                pending.export.exported || explicit_export || explicit_default,
                pending.export.default_export || explicit_default,
            ),
            execution: SymbolExecutionFlags {
                async_symbol: pending.async_symbol,
                static_member: pending.static_member,
            },
            visibility: pending.visibility,
            structural_digest,
            clone_shape_digest,
            clone_token_profile,
        })
    }

    fn emit_reference(&mut self, reference: ExtractedReference) -> Result<(), ExtractError> {
        let reference = ExtractedReference {
            name: self.bound_canonical_name(
                reference.name,
                crate::bounded_name::MAX_CANONICAL_REFERENCE_NAME_BYTES,
            ),
            resolution_name: reference.resolution_name.map(|name| {
                self.bound_canonical_name(
                    name,
                    crate::bounded_name::MAX_CANONICAL_REFERENCE_NAME_BYTES,
                )
            }),
            ..reference
        };
        self.context.budget.reserve_fact(
            reference_budget_bytes(&reference),
            [
                reference
                    .owner
                    .as_ref()
                    .map_or("", cartograph_domain::SymbolId::as_str),
                reference.name.as_str(),
            ],
        )?;
        self.facts.references.push(reference);
        Ok(())
    }

    fn emit_import_binding(&mut self, binding: ExtractedImportBinding) -> Result<(), ExtractError> {
        self.context.budget.reserve_fact(
            import_binding_budget_bytes(&binding),
            [
                binding.module_specifier.as_str(),
                binding.imported_name.as_str(),
                binding.local_name.as_str(),
            ],
        )?;
        self.facts.import_bindings.push(binding);
        Ok(())
    }

    fn qualified_name(&self, name: &str) -> Result<String, ExtractError> {
        let length = self
            .qualifiers
            .iter()
            .try_fold(name.len(), |length, qualifier| {
                length
                    .checked_add(qualifier.len())
                    .and_then(|length| length.checked_add(2))
            });
        let length = length.ok_or(ExtractError::OutputLimit)?;
        self.context.budget.ensure_string_length(length)?;
        let mut qualified = String::new();
        qualified
            .try_reserve(length)
            .map_err(|_| ExtractError::OutputLimit)?;
        for qualifier in &self.qualifiers {
            qualified.push_str(qualifier);
            qualified.push_str("::");
        }
        qualified.push_str(name);
        Ok(qualified)
    }

    /// Shorten one synthesized name that exceeds its canonical storage bound,
    /// recording that this file carries a shortened name.
    fn bound_canonical_name(&mut self, name: String, limit: usize) -> String {
        match crate::bounded_name::shortened_canonical_name(&name, limit) {
            Some(shortened) => {
                self.shortened_canonical_names = true;
                shortened
            }
            None => name,
        }
    }
}

fn emit_javascript_callable_parameters(
    builder: &mut ExtractionBuilder<'_, '_>,
    callable: Node<'_>,
) -> Result<(), ExtractError> {
    let parameters = callable
        .child_by_field_name("parameters")
        .or_else(|| callable.child_by_field_name("parameter"));
    if let Some(parameters) = parameters {
        emit_javascript_binding_tree(builder, parameters, SymbolKind::Parameter)?;
    }
    Ok(())
}

fn emit_javascript_binding_tree(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: SymbolKind,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            let name = builder.context.owned_text(node)?;
            builder.emit_symbol(PendingSymbol {
                kind,
                name,
                span_node: node,
                structural_node: node,
                doc_anchor: node,
                body_node: None,
                declaration_only: false,
                signature: None,
                export: crate::SymbolExportFlags::new(false, false),
                async_symbol: false,
                static_member: false,
                visibility: None,
            })?;
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(binding) = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("pattern"))
            {
                emit_javascript_binding_tree(builder, binding, kind)?;
            }
        }
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                emit_javascript_binding_tree(builder, value, kind)?;
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                emit_javascript_binding_tree(builder, left, kind)?;
            }
        }
        "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
            for child in named_children(node) {
                emit_javascript_binding_tree(builder, child, kind)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl<'source, 'cancel> ExtractionContext<'source, 'cancel> {
    fn new(
        snapshot: &'source SourceSnapshot,
        cancelled: &'cancel mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        Ok(Self {
            snapshot,
            cancelled,
            budget: ExtractionBudget::new(snapshot)?,
        })
    }

    fn ensure_active(&mut self) -> Result<(), ExtractError> {
        if (self.cancelled)() {
            Err(ExtractError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn source(&self) -> &str {
        self.snapshot.source()
    }

    fn owned_text(&mut self, node: Node<'_>) -> Result<String, ExtractError> {
        self.ensure_active()?;
        let raw_length = node.end_byte().saturating_sub(node.start_byte());
        self.budget.ensure_string_length(raw_length)?;
        let value = self.text(node).trim();
        self.copy_text(value)
    }

    fn owned_unquoted_text(&mut self, node: Node<'_>) -> Result<String, ExtractError> {
        self.ensure_active()?;
        let raw_length = node.end_byte().saturating_sub(node.start_byte());
        self.budget.ensure_string_length(raw_length)?;
        let value = syntax::unquote(self.text(node));
        self.copy_text(value)
    }

    fn copy_text(&self, value: &str) -> Result<String, ExtractError> {
        self.budget.ensure_string_length(value.len())?;
        let mut owned = String::new();
        owned
            .try_reserve(value.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        owned.push_str(value);
        Ok(owned)
    }

    fn callable_signature(&mut self, node: Node<'_>) -> Result<Option<String>, ExtractError> {
        let source = self.snapshot.source();
        let signature = callable_signature(node, source, &mut *self.cancelled)?;
        if let Some(value) = &signature {
            self.budget.ensure_string_length(value.len())?;
        }
        Ok(signature)
    }

    fn assignment_signature(&mut self, node: Node<'_>) -> Result<Option<String>, ExtractError> {
        let value = self.owned_text(node)?;
        if !declaration_value_is_search_safe(&value) {
            return Ok(None);
        }
        let length = value
            .len()
            .checked_add(2)
            .ok_or(ExtractError::OutputLimit)?;
        self.budget.ensure_string_length(length)?;
        let mut signature = String::new();
        signature
            .try_reserve(length)
            .map_err(|_| ExtractError::OutputLimit)?;
        signature.push_str("= ");
        signature.push_str(&value);
        Ok(Some(signature))
    }

    fn jsdoc(&mut self, node: Node<'_>) -> Result<Option<String>, ExtractError> {
        if self.snapshot.language() == SourceLanguage::GraphQl {
            return graphql_family::description_from_context(self, node);
        }
        let source = self.snapshot.source();
        let docstring = jsdoc(node, source, &mut *self.cancelled)?;
        if let Some(value) = &docstring {
            self.budget.ensure_string_length(value.len())?;
        }
        Ok(docstring)
    }

    fn text(&self, node: Node<'_>) -> &str {
        self.source()
            .get(node.start_byte()..node.end_byte())
            .unwrap_or_default()
    }
}
