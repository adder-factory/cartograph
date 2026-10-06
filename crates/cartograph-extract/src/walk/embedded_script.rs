//! JavaScript-family script regions embedded in component files.
//!
//! Astro frontmatter, Vue and Svelte `<script>` blocks, and template
//! expressions are parsed by the native JavaScript/TypeScript grammar restricted
//! to their exact byte range with tree-sitter included ranges over the full host
//! bytes. Every node then already carries host byte offsets and host
//! line/column positions, so no span is remapped after the fact.
//!
//! The regions are walked by the ordinary JavaScript-family walker over a
//! dialect view of the host snapshot, sharing the host's symbol ordinals and
//! per-file output budget. Declarations keep module-scope qualified names, as in
//! a standalone script, while the file-level component is their graph parent and
//! the owner of top-level references. Script programs run the complete walker
//! pipeline. Template expressions run only the usage traversal: an inline
//! handler's or callback's own declarations are walked for their references but
//! declare nothing, every reference they make belongs to the component, names
//! they bind for themselves are never resolved against the script, and a
//! template with many expressions costs time proportional to the expressions
//! alone.

use cartograph_domain::{SourceLanguage, SymbolId};
use tree_sitter::{Node, Parser, Range, Tree};

use crate::{
    Containment, DiagnosticCode, ExtractError, ExtractedImportBinding, ExtractedReference,
    ExtractedSymbol, ExtractionDiagnostic, NativeGrammar, SourceSnapshot, budget::ExtractionBudget,
    identity::SymbolIdentity, native::parse_with_cancellation, source_lines::LineMap,
};

use super::{
    ExtractionBuilder, enrich_visited, module_system::CommonJsShadowing, prepare_extraction,
    syntax::collect_diagnostics,
};

mod component_conventions;
mod template_locals;

use component_conventions::{
    CompilerNames, ProgramImports, anchor_static_imports, drop_compiler_references,
    emit_store_subscriptions,
};
pub(crate) use component_conventions::{SVELTE_RUNES, VUE_COMPILER_MACROS};
use template_locals::{ExpressionStart, drop_template_local_facts};

/// Syntax diagnostics retained across all programs of one file; matches the
/// per-tree diagnostic ceiling of a standalone parse.
const MAX_EMBEDDED_DIAGNOSTICS: usize = 32;
/// Script programs per dialect that also run the whole-file enrichers (value
/// references, schema recognizers, embedded SQL, dispatch tables). Each of
/// those passes costs time proportional to the whole file or its declarations,
/// so later programs of a pathological file keep their declarations and
/// references without them.
const MAX_ENRICHED_PROGRAMS: usize = 16;
/// Owner-stack depth of an embedded module's top level: the component.
const COMPONENT_MODULE_SCOPE_OWNERS: usize = 1;

/// Embedded-region state of a walker builder; empty for a standalone file.
#[derive(Default)]
pub(super) struct EmbeddedScope {
    /// Diagnostics of embedded regions, already in host spans.
    pub(super) diagnostics: Vec<ExtractionDiagnostic>,
    /// Owners enclosing the module top level: none for a standalone file, the
    /// file-level component for an embedded component script.
    pub(super) module_scope_owners: usize,
    /// Whether the walk is inside a template expression, whose declarations
    /// are traversed for their references but never emitted.
    pub(super) usage_only: bool,
}

impl EmbeddedScope {
    /// The owner of module-level code: the file-level component of an embedded
    /// script, or none in a standalone file.
    pub(super) fn module_owner(&self, owners: &[SymbolId]) -> Option<SymbolId> {
        owners
            .get(..self.module_scope_owners)
            .and_then(<[SymbolId]>::last)
            .cloned()
    }
}

/// Grammar dialect one embedded region is parsed and walked as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScriptDialect {
    /// TypeScript without JSX.
    TypeScript,
    /// TypeScript with JSX (`<script lang="tsx">`).
    Tsx,
    /// JavaScript, which also admits JSX.
    JavaScript,
}

impl ScriptDialect {
    const ALL: [Self; 3] = [Self::TypeScript, Self::Tsx, Self::JavaScript];

    const fn language(self) -> SourceLanguage {
        match self {
            Self::TypeScript => SourceLanguage::TypeScript,
            Self::Tsx => SourceLanguage::Tsx,
            Self::JavaScript => SourceLanguage::JavaScript,
        }
    }

    const fn grammar(self) -> NativeGrammar {
        match self {
            Self::TypeScript => NativeGrammar::TypeScript,
            Self::Tsx => NativeGrammar::Tsx,
            Self::JavaScript => NativeGrammar::JavaScript,
        }
    }

    /// Dialect named by a `<script lang="…">` value; absent or other values are JavaScript.
    pub(crate) fn from_lang_attribute(lang: Option<&str>) -> Self {
        let Some(lang) = lang.map(str::trim) else {
            return Self::JavaScript;
        };
        if lang.eq_ignore_ascii_case("ts") || lang.eq_ignore_ascii_case("typescript") {
            Self::TypeScript
        } else if lang.eq_ignore_ascii_case("tsx") {
            Self::Tsx
        } else {
            Self::JavaScript
        }
    }

    /// Whether the dialect admits TypeScript syntax.
    pub(crate) const fn is_typed(self) -> bool {
        matches!(self, Self::TypeScript | Self::Tsx)
    }
}

/// Whether a `<script type="…">` value denotes executable JavaScript/TypeScript.
///
/// Data blocks such as `application/ld+json` or client templates are not code
/// and are never parsed as a program.
pub(crate) fn script_type_is_code(script_type: Option<&str>) -> bool {
    let Some(script_type) = script_type.map(str::trim) else {
        return true;
    };
    script_type.is_empty()
        || [
            "module",
            "text/javascript",
            "application/javascript",
            "text/ecmascript",
            "application/ecmascript",
            "text/typescript",
            "application/typescript",
        ]
        .iter()
        .any(|code_type| script_type.eq_ignore_ascii_case(code_type))
}

/// Syntactic role of one embedded region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegionRole {
    /// A whole script program; its syntax errors make the host file partial.
    Program,
    /// One template expression; never affects the host parse status.
    TemplateExpression,
}

/// One delimited script region in exact host byte offsets.
///
/// A region is one or more ordered, disjoint host byte ranges parsed as a
/// single token stream; an Astro expression interleaved with markup keeps only
/// its script pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScriptRegion {
    pieces: Vec<(usize, usize)>,
    dialect: ScriptDialect,
    role: RegionRole,
}

impl ScriptRegion {
    /// A whole script program between `start` and `end`.
    pub(crate) fn program(start: usize, end: usize, dialect: ScriptDialect) -> Self {
        Self {
            pieces: vec![(start, end)],
            dialect,
            role: RegionRole::Program,
        }
    }

    /// One template expression between `start` and `end`.
    pub(crate) fn template_expression(start: usize, end: usize, dialect: ScriptDialect) -> Self {
        Self::template_pieces(vec![(start, end)], dialect)
    }

    /// One template expression made of ordered script pieces.
    pub(crate) fn template_pieces(pieces: Vec<(usize, usize)>, dialect: ScriptDialect) -> Self {
        Self {
            pieces,
            dialect,
            role: RegionRole::TemplateExpression,
        }
    }

    /// Host byte offset where the region starts.
    pub(crate) fn start(&self) -> usize {
        self.pieces.first().map_or(0, |(start, _)| *start)
    }

    /// Grammar dialect of the region.
    pub(crate) const fn dialect(&self) -> ScriptDialect {
        self.dialect
    }

    fn is_empty(&self) -> bool {
        self.pieces.iter().all(|(start, end)| start >= end)
    }

    /// Whether every piece is a host range on character boundaries, in order.
    fn is_valid_in(&self, source: &str) -> bool {
        let mut previous_end = 0;
        self.pieces.iter().all(|(start, end)| {
            let valid = previous_end <= *start
                && start <= end
                && source.is_char_boundary(*start)
                && source.is_char_boundary(*end);
            previous_end = *end;
            valid
        })
    }
}

/// Framework conventions applied to every region of one component file.
#[derive(Clone, Copy)]
pub(crate) struct ComponentScriptPolicy {
    /// Compiler-provided names whose invocations are not project references.
    compiler_names: &'static [&'static str],
    /// Whether `$name` identifiers subscribe to the Svelte store `name`.
    store_subscriptions: bool,
}

/// Vue single-file components: compiler macros are not calls.
pub(crate) const VUE_SCRIPT_POLICY: ComponentScriptPolicy = ComponentScriptPolicy {
    compiler_names: VUE_COMPILER_MACROS,
    store_subscriptions: false,
};

/// Svelte components: runes are not calls and `$store` subscribes to `store`.
pub(crate) const SVELTE_SCRIPT_POLICY: ComponentScriptPolicy = ComponentScriptPolicy {
    compiler_names: SVELTE_RUNES,
    store_subscriptions: true,
};

/// Astro components: frontmatter and expressions are plain TypeScript.
pub(crate) const ASTRO_SCRIPT_POLICY: ComponentScriptPolicy = ComponentScriptPolicy {
    compiler_names: &[],
    store_subscriptions: false,
};

/// The host file-level component that owns every embedded fact.
#[derive(Clone, Copy)]
pub(crate) struct EmbeddedComponent<'host> {
    /// Host snapshot whose bytes the regions index.
    pub(crate) snapshot: &'host SourceSnapshot,
    /// File-level component symbol: graph parent of embedded declarations.
    pub(crate) id: &'host SymbolId,
    /// Framework conventions of the host language.
    pub(crate) policy: ComponentScriptPolicy,
    /// Project AST-depth ceiling of the host extraction.
    pub(crate) maximum_ast_depth: usize,
}

/// Host extraction state lent to the embedded walk and returned afterwards.
pub(crate) struct EmbeddedLease<'lease, 'path> {
    /// Host symbol identities; embedded symbols continue their ordinals.
    pub(crate) identities: &'lease mut SymbolIdentity<'path>,
    /// Host per-file output budget; embedded facts are charged to it.
    pub(crate) budget: &'lease mut ExtractionBudget,
}

/// Every region of one component file and the state they share.
pub(crate) struct EmbeddedRequest<'request, 'lease, 'path> {
    /// Owning component.
    pub(crate) component: EmbeddedComponent<'request>,
    /// Regions in document order.
    pub(crate) regions: &'request [ScriptRegion],
    /// Host state lent to the walk.
    pub(crate) lease: EmbeddedLease<'lease, 'path>,
}

/// One static `import … from "module"` of a script program.
pub(crate) struct EmbeddedImportSite {
    /// Unquoted module specifier.
    pub(crate) module: String,
    /// Host byte offset of the specifier text.
    pub(crate) start: usize,
    /// Host byte end of the specifier text.
    pub(crate) end: usize,
}

/// Facts of every region, in host coordinates and already charged to the host budget.
///
/// JavaScript-family dialects emit no numerical sites, which are Rust-only.
#[derive(Default)]
pub(crate) struct EmbeddedFacts {
    /// Declarations, owned by the component or by other embedded declarations.
    pub(crate) symbols: Vec<ExtractedSymbol>,
    /// Parent/child edges, including component-to-declaration edges.
    pub(crate) containments: Vec<Containment>,
    /// References of every region.
    pub(crate) references: Vec<ExtractedReference>,
    /// Module bindings of every script program.
    pub(crate) import_bindings: Vec<ExtractedImportBinding>,
    /// Diagnostics of regions that lost facts: script syntax errors and template
    /// expressions too deep to walk (not yet charged to the budget).
    pub(crate) diagnostics: Vec<ExtractionDiagnostic>,
    /// Static import specifiers of script programs.
    pub(crate) import_sites: Vec<EmbeddedImportSite>,
    /// Whether a synthesized name had to be shortened.
    pub(crate) shortened_canonical_names: bool,
}

impl EmbeddedFacts {
    /// Record, once per file, that unterminated template structure used up
    /// the template scan budget, so later expressions and tags were found
    /// without lexical structure: the file is partial.
    pub(crate) fn record_truncated_template(&mut self) {
        record_once(&mut self.diagnostics, DiagnosticCode::SyntaxError);
    }
}

/// Add a span-less diagnostic of `code` unless one is already recorded or
/// the per-file diagnostic ceiling is reached.
fn record_once(diagnostics: &mut Vec<ExtractionDiagnostic>, code: DiagnosticCode) {
    let reported = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == code && diagnostic.span.is_none());
    if !reported && diagnostics.len() < MAX_EMBEDDED_DIAGNOSTICS {
        diagnostics.push(ExtractionDiagnostic { code, span: None });
    }
}

/// Walk every region of one component file with the JavaScript-family walker.
/// # Errors
///
/// Returns an error on cancellation, an invalid region, grammar or parser
/// failure, nesting overflow, or an exhausted output budget.
pub(crate) fn extract_component_scripts(
    request: EmbeddedRequest<'_, '_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<EmbeddedFacts, ExtractError> {
    let EmbeddedRequest {
        component,
        regions,
        mut lease,
    } = request;
    let mut facts = EmbeddedFacts::default();
    if regions.is_empty() {
        return Ok(facts);
    }
    let source = component.snapshot.source();
    if !regions.iter().all(|region| region.is_valid_in(source)) {
        return Err(ExtractError::InvalidSpan);
    }
    let lines = LineMap::new(source)?;
    for dialect in ScriptDialect::ALL {
        if regions.iter().any(|region| region.dialect == dialect) {
            DialectPass {
                component: &component,
                lines: &lines,
                regions,
                dialect,
                lease: &mut lease,
                facts: &mut facts,
            }
            .run(cancelled)?;
        }
    }
    Ok(facts)
}

/// All regions of one dialect, walked by one builder over one dialect view.
struct DialectPass<'pass, 'lease, 'path> {
    component: &'pass EmbeddedComponent<'pass>,
    lines: &'pass LineMap,
    regions: &'pass [ScriptRegion],
    dialect: ScriptDialect,
    lease: &'pass mut EmbeddedLease<'lease, 'path>,
    facts: &'pass mut EmbeddedFacts,
}

impl DialectPass<'_, '_, '_> {
    fn run(self, cancelled: &mut dyn FnMut() -> bool) -> Result<(), ExtractError> {
        let view = self
            .component
            .snapshot
            .dialect_view(self.dialect.language())?;
        let mut parser = Parser::new();
        parser
            .set_language(&self.dialect.grammar().language())
            .map_err(|_| ExtractError::GrammarUnavailable)?;
        let mut builder =
            ExtractionBuilder::new(&view, self.component.maximum_ast_depth, cancelled)?;
        exchange_host_state(&mut builder, &mut *self.lease);
        let walked = RegionWalker {
            builder: &mut builder,
            parser: &mut parser,
            component: self.component,
            lines: self.lines,
            facts: &mut *self.facts,
        }
        .walk_dialect(self.regions, self.dialect);
        exchange_host_state(&mut builder, &mut *self.lease);
        walked?;
        absorb_builder(builder, self.facts);
        Ok(())
    }
}

/// Forget the export lists and `CommonJS` shadowing of the previous program.
fn reset_module_scope(builder: &mut ExtractionBuilder<'_, '_>) {
    builder.explicit_exports.clear();
    builder.explicit_default_exports.clear();
    builder.commonjs_shadowing = CommonJsShadowing::default();
}

/// Swap the lent host budget and symbol ordinals into or back out of `builder`.
fn exchange_host_state(builder: &mut ExtractionBuilder<'_, '_>, lease: &mut EmbeddedLease<'_, '_>) {
    std::mem::swap(&mut builder.context.budget, lease.budget);
    builder.identities.exchange_ordinals(lease.identities);
}

/// Move the walked facts out of `builder`. The component is the file's default
/// export, so no embedded declaration competes for that role.
fn absorb_builder(builder: ExtractionBuilder<'_, '_>, facts: &mut EmbeddedFacts) {
    let mut walked = builder.facts;
    for symbol in &mut walked.symbols {
        symbol.export.default_export = false;
    }
    facts.symbols.append(&mut walked.symbols);
    facts.containments.append(&mut walked.containments);
    facts.references.append(&mut walked.references);
    facts.import_bindings.append(&mut walked.import_bindings);
    facts.shortened_canonical_names |= builder.shortened_canonical_names;
}

/// Fact counts and scope depths before one template expression, restored
/// when the expression is dropped.
struct ExpressionMark {
    start: ExpressionStart,
    owners: usize,
    qualifiers: usize,
}

impl ExpressionMark {
    fn of(builder: &ExtractionBuilder<'_, '_>) -> Self {
        Self {
            start: ExpressionStart {
                reference: builder.facts.references.len(),
                binding: builder.facts.import_bindings.len(),
            },
            owners: builder.owners.len(),
            qualifiers: builder.qualifiers.len(),
        }
    }

    /// Drop everything the expression emitted. A usage-only walk emits no
    /// declarations, so references and dynamic-import bindings are all of it.
    fn restore(&self, builder: &mut ExtractionBuilder<'_, '_>) {
        builder.facts.references.truncate(self.start.reference);
        builder.facts.import_bindings.truncate(self.start.binding);
        builder.owners.truncate(self.owners);
        builder.qualifiers.truncate(self.qualifiers);
    }
}

struct RegionWalker<'walk, 'source, 'cancel> {
    builder: &'walk mut ExtractionBuilder<'source, 'cancel>,
    parser: &'walk mut Parser,
    component: &'walk EmbeddedComponent<'walk>,
    lines: &'walk LineMap,
    facts: &'walk mut EmbeddedFacts,
}

impl RegionWalker<'_, '_, '_> {
    fn walk_dialect(
        &mut self,
        regions: &[ScriptRegion],
        dialect: ScriptDialect,
    ) -> Result<(), ExtractError> {
        self.builder.owners.push(self.component.id.clone());
        self.builder.embedded.module_scope_owners = COMPONENT_MODULE_SCOPE_OWNERS;
        let of_role = |role: RegionRole| {
            regions.iter().filter(move |region| {
                region.dialect == dialect && region.role == role && !region.is_empty()
            })
        };
        let mut programs = Vec::new();
        for region in of_role(RegionRole::Program) {
            programs.push(self.parse(region)?);
        }
        self.walk_programs(&programs)?;
        for region in of_role(RegionRole::TemplateExpression) {
            let tree = self.parse(region)?;
            self.walk_expression(tree.root_node())?;
        }
        self.builder.owners.pop();
        Ok(())
    }

    fn parse(&mut self, region: &ScriptRegion) -> Result<Tree, ExtractError> {
        // A tiny region parses without reaching tree-sitter's progress
        // callback, so a run of them is polled here.
        self.builder.context.ensure_active()?;
        let ranges = region
            .pieces
            .iter()
            .filter(|(start, end)| start < end)
            .map(|(start, end)| Range {
                start_byte: *start,
                end_byte: *end,
                start_point: self.lines.point(*start),
                end_point: self.lines.point(*end),
            })
            .collect::<Vec<_>>();
        self.parser
            .set_included_ranges(&ranges)
            .map_err(|_| ExtractError::InvalidSpan)?;
        let snapshot = self.builder.context.snapshot;
        parse_with_cancellation(
            self.parser,
            snapshot.source().as_bytes(),
            &mut *self.builder.context.cancelled,
        )
    }

    /// Walk every program of the dialect. Each `<script>` block is its own
    /// module for export lists (restored for its enrichment pass too), while
    /// the enrichers run after every block is visited, so they see every
    /// declaration regardless of which block declared it.
    fn walk_programs(&mut self, programs: &[Tree]) -> Result<(), ExtractError> {
        let first_reference = self.builder.facts.references.len();
        let first_binding = self.builder.facts.import_bindings.len();
        for program in programs {
            reset_module_scope(self.builder);
            prepare_extraction(self.builder, program.root_node())?;
            self.builder.visit(program.root_node(), 0)?;
        }
        for program in programs.iter().take(MAX_ENRICHED_PROGRAMS) {
            reset_module_scope(self.builder);
            prepare_extraction(self.builder, program.root_node())?;
            enrich_visited(self.builder, program.root_node())?;
        }
        let roots = programs.iter().map(Tree::root_node).collect::<Vec<_>>();
        self.apply_conventions(&roots, first_reference)?;
        for root in &roots {
            self.record_syntax_diagnostics(*root)?;
        }
        let sites = anchor_static_imports(
            self.builder,
            &ProgramImports {
                roots: &roots,
                first_reference,
                first_binding,
            },
        )?;
        self.facts.import_sites.extend(sites);
        Ok(())
    }

    /// Walk one template expression for its calls, constructions, component
    /// uses, and member accesses. An expression nested past the AST-depth
    /// ceiling loses only its own references and marks the file partial; the
    /// rest of the component keeps its facts.
    fn walk_expression(&mut self, root: Node<'_>) -> Result<(), ExtractError> {
        let mark = ExpressionMark::of(self.builder);
        match self.walk_expression_usage(root, mark.start) {
            Err(ExtractError::NestingLimit) => {
                mark.restore(self.builder);
                self.record_deep_expression();
                Ok(())
            }
            walked => walked,
        }
    }

    fn walk_expression_usage(
        &mut self,
        root: Node<'_>,
        start: ExpressionStart,
    ) -> Result<(), ExtractError> {
        self.builder.javascript.scopes = super::javascript_scopes::LexicalScopes::default();
        self.builder.embedded.usage_only = true;
        let visited = self.builder.visit(root, 0);
        self.builder.embedded.usage_only = false;
        visited?;
        drop_template_local_facts(self.builder, root, start)?;
        self.apply_conventions(&[root], start.reference)
    }

    /// Report, once per file, that a template expression was too deep to walk.
    fn record_deep_expression(&mut self) {
        record_once(
            &mut self.facts.diagnostics,
            DiagnosticCode::NestingLimitExceeded,
        );
    }

    fn apply_conventions(
        &mut self,
        roots: &[Node<'_>],
        first_reference: usize,
    ) -> Result<(), ExtractError> {
        let policy = self.component.policy;
        if !policy.compiler_names.is_empty() {
            drop_compiler_references(
                self.builder,
                roots,
                CompilerNames {
                    names: policy.compiler_names,
                    first_reference,
                },
            )?;
        }
        if policy.store_subscriptions {
            for root in roots {
                emit_store_subscriptions(self.builder, *root, self.component.id)?;
            }
        }
        Ok(())
    }

    fn record_syntax_diagnostics(&mut self, root: Node<'_>) -> Result<(), ExtractError> {
        let remaining = MAX_EMBEDDED_DIAGNOSTICS.saturating_sub(self.facts.diagnostics.len());
        if !root.has_error() || remaining == 0 {
            return Ok(());
        }
        let mut diagnostics = collect_diagnostics(root, &mut *self.builder.context.cancelled)?;
        if diagnostics.is_empty() {
            diagnostics.push(ExtractionDiagnostic {
                code: DiagnosticCode::SyntaxError,
                span: None,
            });
        }
        diagnostics.truncate(remaining);
        self.facts.diagnostics.append(&mut diagnostics);
        Ok(())
    }
}
