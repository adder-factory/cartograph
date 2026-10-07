mod bridge_transaction;
mod cargo_path_bindings;
pub(crate) mod literal_bindings;
mod fwjs;
mod owner_index;

use bridge_transaction::BridgeTransaction;
pub(crate) use fwjs::member_call_is_syntax;

use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

use cartograph_domain::{
    ContentDigest, ReferenceKind, SourceLanguage, SourcePosition, SourceSpan, SymbolId, SymbolKind,
    Visibility,
};

use crate::{
    Containment, ExtractError, ExtractedFile, ExtractedReference, ExtractedSymbol, SourceSnapshot,
    SymbolExecutionFlags, SymbolExportFlags, SymbolImplementationFlags,
    budget::{containment_budget_bytes, reference_budget_bytes, symbol_budget_bytes},
    source_lines::{LineMap, SourceByteRange, physical_lines},
};

const FRAMEWORK_SYMBOL_DOMAIN: &str = "cartograph.v2.framework-symbol.2026-07-24";
const FRAMEWORK_DIGEST_DOMAIN: &str = "cartograph.v2.framework-digest.2026-07-24";
const LANDMARK_SITE_INDEX_ENTRY_BYTES: u64 = 128;
const CANCELLATION_INTERVAL_FRAMEWORK_ITEMS: usize = 256;
type ReferenceSite = (Option<SymbolId>, ReferenceKind, String, u64, u64);
const MAX_ROUTE_BYTES: usize = 1_024;
const MAX_SIGNAL_BYTES: usize = 4_096;
const PYTHON_MULTILINE_DELIMITER_BYTES: usize = 3;

pub(crate) fn skip_ascii_whitespace(value: &str, mut cursor: usize) -> usize {
    while value
        .as_bytes()
        .get(cursor)
        .is_some_and(u8::is_ascii_whitespace)
    {
        cursor += 1;
    }
    cursor
}

pub(crate) fn javascript_identifier_at(value: &str, start: usize) -> Option<(usize, &str)> {
    let first = *value.as_bytes().get(start)?;
    if !(first == b'_' || first == b'$' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = start + 1;
    while value
        .as_bytes()
        .get(end)
        .is_some_and(|byte| *byte == b'_' || *byte == b'$' || byte.is_ascii_alphanumeric())
    {
        end += 1;
    }
    Some((end, &value[start..end]))
}

pub(crate) struct FrameworkInput<'source> {
    snapshot: &'source SourceSnapshot,
    file: ExtractedFile,
    root: Option<Node<'source>>,
}

impl<'source> FrameworkInput<'source> {
    pub(crate) fn new(snapshot: &'source SourceSnapshot, file: ExtractedFile) -> Self {
        Self {
            snapshot,
            file,
            root: None,
        }
    }

    pub(crate) fn with_root(mut self, root: Node<'source>) -> Self {
        self.root = Some(root);
        self
    }
}

pub(crate) fn enrich(
    input: FrameworkInput<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ExtractedFile, ExtractError> {
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    let masked_source = mask_comments(
        input.snapshot.source(),
        input.snapshot.language(),
        cancelled,
    )?;
    let mut builder = FrameworkBuilder::new(input, cancelled)?;
    cargo_path_bindings::extract(&mut builder)?;
    crate::framework_bun::scan(&mut builder, &masked_source)?;
    crate::framework_codeigniter::scan(&mut builder, &masked_source)?;
    crate::framework_drupal::scan(&mut builder, &masked_source)?;
    crate::framework_hono::scan(&mut builder, &masked_source)?;
    crate::framework_managed_routes::scan(&mut builder, &masked_source)?;
    crate::framework_manifest::scan(&mut builder, &masked_source)?;
    crate::framework_mybatis::scan(&mut builder, &masked_source)?;
    crate::framework_nest::scan(&mut builder, &masked_source)?;
    crate::framework_rails::scan(&mut builder, &masked_source)?;
    crate::framework_salesforce::scan(&mut builder)?;
    crate::framework_spring::scan(&mut builder, &masked_source)?;
    crate::framework_symfony::scan(&mut builder, &masked_source)?;
    scan_framework_signals(&mut builder, &masked_source)?;
    crate::framework_bridge::scan(&mut builder, &masked_source)?;
    builder.finish()
}

pub(crate) struct FrameworkBuilder<'source, 'cancel> {
    snapshot: &'source SourceSnapshot,
    root: Option<Node<'source>>,
    pub(crate) bridge: BridgeTransaction<'cancel>,
    lines: LineMap,
    landmark_sites: BTreeSet<(SymbolKind, String, usize, usize)>,
    symbol_keys: BTreeSet<(SymbolKind, String)>,
    reference_keys: BTreeMap<ReferenceSite, usize>,
    ordinals: BTreeMap<(SymbolKind, String), u64>,
    /// `@Value(...)` argument ranges and the declarations they decorate,
    /// sorted by start; recorded by the Spring scanner before config scanning.
    value_annotations: Vec<AnnotationInterval>,
}

/// The argument range of one annotation and the declaration it decorates.
pub(crate) struct AnnotationInterval {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) owner: SymbolId,
}

pub(crate) struct LandmarkInput<'target> {
    pub(crate) kind: SymbolKind,
    pub(crate) name: String,
    pub(crate) identity: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) body_search_text: String,
    pub(crate) target: Option<(&'target str, Option<&'target str>, usize, usize)>,
}

#[derive(Clone, Copy)]
pub(crate) struct FrameworkRouteInput<'target> {
    pub(crate) method: &'target str,
    pub(crate) path: &'target str,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) command: bool,
    pub(crate) handler: Option<(&'target str, usize, usize)>,
}

#[derive(Clone, Copy)]
pub(crate) struct FrameworkResolvedRouteInput<'target> {
    pub(crate) method: &'target str,
    pub(crate) path: &'target str,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) command: bool,
    pub(crate) target: Option<(&'target str, Option<&'target str>, usize, usize)>,
}

pub(crate) struct FrameworkReferenceInput<'reference> {
    pub(crate) owner: Option<SymbolId>,
    pub(crate) name: &'reference str,
    pub(crate) resolution_name: Option<&'reference str>,
    pub(crate) kind: ReferenceKind,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct FrameworkNearReferenceInput<'reference> {
    pub(crate) name: &'reference str,
    pub(crate) resolution_name: Option<&'reference str>,
    pub(crate) kind: ReferenceKind,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Copy)]
struct FrameworkSignalReferenceInput<'reference> {
    name: &'reference str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct DelimiterInput<'source> {
    value: &'source str,
    open: usize,
    opening: u8,
    closing: u8,
    limit: usize,
}

impl<'source> DelimiterInput<'source> {
    pub(crate) const fn parentheses(value: &'source str, open: usize) -> Self {
        Self {
            value,
            open,
            opening: b'(',
            closing: b')',
            limit: value.len(),
        }
    }

    pub(crate) const fn braces(value: &'source str, open: usize) -> Self {
        Self {
            value,
            open,
            opening: b'{',
            closing: b'}',
            limit: value.len(),
        }
    }

    pub(crate) const fn square_brackets(value: &'source str, open: usize) -> Self {
        Self {
            value,
            open,
            opening: b'[',
            closing: b']',
            limit: value.len(),
        }
    }

    pub(crate) fn bounded_parentheses(
        value: &'source str,
        open: usize,
        maximum_bytes: usize,
    ) -> Self {
        Self {
            value,
            open,
            opening: b'(',
            closing: b')',
            limit: value.len().min(open.saturating_add(maximum_bytes)),
        }
    }
}

#[derive(Clone, Copy)]
struct RouteStatementInput<'source> {
    hints: FrameworkHints,
    start: usize,
    statement: &'source str,
}

const ROUTE_MARKERS: &[(&str, &str, bool)] = &[
    ("Route::get(", "GET", false),
    ("Route::post(", "POST", false),
    ("Route::put(", "PUT", false),
    ("Route::patch(", "PATCH", false),
    ("Route::delete(", "DELETE", false),
    ("Route::options(", "OPTIONS", false),
    ("Route::any(", "ANY", false),
    ("->get(", "GET", false),
    ("->post(", "POST", false),
    ("->put(", "PUT", false),
    ("->patch(", "PATCH", false),
    ("->delete(", "DELETE", false),
    ("MapGet(", "GET", false),
    ("MapPost(", "POST", false),
    ("MapPut(", "PUT", false),
    ("MapPatch(", "PATCH", false),
    ("MapDelete(", "DELETE", false),
    ("HandleFunc(", "ANY", false),
    ("path(", "ANY", false),
    ("re_path(", "ANY", false),
    (".GET(", "GET", false),
    (".POST(", "POST", false),
    (".PUT(", "PUT", false),
    (".PATCH(", "PATCH", false),
    (".DELETE(", "DELETE", false),
    (".OPTIONS(", "OPTIONS", false),
    (".HEAD(", "HEAD", false),
    (".Get(", "GET", true),
    (".Post(", "POST", true),
    (".Put(", "PUT", true),
    (".Patch(", "PATCH", true),
    (".Delete(", "DELETE", true),
    (".Options(", "OPTIONS", true),
    (".Head(", "HEAD", true),
    (".get(", "GET", false),
    (".post(", "POST", false),
    (".put(", "PUT", false),
    (".patch(", "PATCH", false),
    (".delete(", "DELETE", false),
    (".options(", "OPTIONS", false),
    (".head(", "HEAD", false),
    (".connect(", "CONNECT", false),
    (".trace(", "TRACE", false),
    (".all(", "ALL", false),
    (".use(", "USE", true),
    ("url(", "ANY", false),
];

const HONO_GENERIC_ROUTE_MARKERS: &[&str] = &[
    ".get(",
    ".post(",
    ".put(",
    ".patch(",
    ".delete(",
    ".options(",
    ".head(",
    ".connect(",
    ".trace(",
    ".all(",
    ".use(",
];

#[derive(Clone, Copy)]
struct FrameworkSymbolIdentity<'identity> {
    path: &'identity str,
    kind: SymbolKind,
    qualified_name: &'identity str,
    ordinal: u64,
}

struct ReservedLandmarkIdentity {
    source_identity: String,
    qualified_name: String,
    ordinal: u64,
}

/// Reject unsafe retained source labels and already recorded landmark sites.
fn landmark_is_rejected(builder: &FrameworkBuilder<'_, '_>, input: &LandmarkInput<'_>) -> bool {
    [&input.name, &input.body_search_text]
        .into_iter()
        .flat_map(|text| text.split_whitespace())
        .any(crate::walk::specifier_safety::specifier_may_carry_credential)
        || builder.landmark_sites.contains(&(
            input.kind,
            input.identity.clone(),
            input.start,
            input.end,
        ))
}

fn reserve_landmark_identity(
    builder: &mut FrameworkBuilder<'_, '_>,
    kind: SymbolKind,
    source_identity: &str,
) -> Option<ReservedLandmarkIdentity> {
    let base_qualified = format!("{}::{source_identity}", builder.path());
    let key = (kind, base_qualified.clone());
    let ordinal = *builder.ordinals.entry(key.clone()).or_default();
    let qualified_name = if ordinal == 0 {
        base_qualified
    } else {
        format!("{base_qualified}#{ordinal}")
    };
    *builder.ordinals.entry(key).or_default() = ordinal.saturating_add(1);
    builder
        .symbol_keys
        .insert((kind, qualified_name.clone()))
        .then(|| ReservedLandmarkIdentity {
            source_identity: source_identity.to_owned(),
            qualified_name,
            ordinal,
        })
}

fn landmark_span(
    builder: &FrameworkBuilder<'_, '_>,
    start: usize,
    end: usize,
) -> Result<SourceSpan, ExtractError> {
    if start == end && builder.source().is_empty() {
        return Ok(SourceSpan::synthetic(
            SourcePosition::new(0, 1, 0).map_err(|_| ExtractError::InvalidSpan)?,
        ));
    }
    builder
        .lines
        .span(SourceByteRange::new(start, end, builder.source().len()))
}

fn add_landmark_containment(
    builder: &mut FrameworkBuilder<'_, '_>,
    owner: Option<SymbolId>,
    id: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(parent) = owner else {
        return Ok(());
    };
    let containment = Containment {
        parent,
        child: id.clone(),
    };
    builder
        .bridge
        .budget
        .reserve_fact(containment_budget_bytes(&containment), std::iter::empty())?;
    builder.bridge.file.containments.push(containment);
    Ok(())
}

impl<'source, 'cancel> FrameworkBuilder<'source, 'cancel> {
    fn new(
        input: FrameworkInput<'source>,
        cancelled: &'cancel mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        let FrameworkInput {
            snapshot,
            file,
            root,
        } = input;
        let mut bridge = BridgeTransaction::new(snapshot, file, cancelled)?;
        if !snapshot.source().is_empty() {
            bridge.index_owners()?;
        }
        let symbol_keys = bridge
            .file
            .symbols
            .iter()
            .map(|symbol| (symbol.kind, symbol.qualified_name.clone()))
            .collect();
        let reference_keys = index_reference_sites(&bridge.file, bridge.cancelled)?;
        Ok(Self {
            snapshot,
            root,
            bridge,
            lines: LineMap::new(snapshot.source())?,
            landmark_sites: BTreeSet::new(),
            symbol_keys,
            reference_keys,
            ordinals: BTreeMap::new(),
            value_annotations: Vec::new(),
        })
    }

    pub(crate) fn check_cancelled(&mut self) -> Result<(), ExtractError> {
        self.bridge.check_cancelled()
    }

    pub(crate) fn source(&self) -> &'source str {
        self.snapshot.source()
    }

    pub(crate) fn path(&self) -> &str {
        self.snapshot.path().as_str()
    }

    pub(crate) fn language(&self) -> SourceLanguage {
        self.snapshot.language()
    }

    pub(crate) fn original_symbol(&self, index: usize) -> Option<&ExtractedSymbol> {
        (index < self.bridge.original_symbols)
            .then(|| self.bridge.file.symbols.get(index))
            .flatten()
    }

    pub(crate) const fn original_symbol_count(&self) -> usize {
        self.bridge.original_symbols
    }

    pub(crate) fn syntax_root(&self) -> Option<Node<'source>> {
        self.root
    }

    pub(crate) fn containments(&self) -> &[Containment] {
        &self.bridge.file.containments
    }

    /// References recorded so far (walker facts first, then framework facts).
    pub(crate) fn references(&self) -> &[ExtractedReference] {
        &self.bridge.file.references
    }

    /// Record the `@Value` annotation ranges of this file.
    pub(crate) fn set_value_annotations(&mut self, mut intervals: Vec<AnnotationInterval>) {
        intervals.sort_by_key(|interval| (interval.start, interval.end));
        self.value_annotations = intervals;
    }

    /// `@Value` annotation ranges, sorted by start.
    pub(crate) fn value_annotations(&self) -> &[AnnotationInterval] {
        &self.value_annotations
    }

    /// Number of references currently retained for this file.
    pub(crate) fn reference_count(&self) -> usize {
        self.bridge.file.references.len()
    }

    /// One retained reference, including those the language walker extracted.
    pub(crate) fn reference(&self, index: usize) -> Option<&ExtractedReference> {
        self.bridge.file.references.get(index)
    }

    pub(crate) fn add_route(&mut self, input: FrameworkRouteInput<'_>) -> Result<(), ExtractError> {
        self.add_route_with_target(FrameworkResolvedRouteInput {
            method: input.method,
            path: input.path,
            start: input.start,
            end: input.end,
            command: input.command,
            target: input
                .handler
                .map(|(name, start, end)| (name, None, start, end)),
        })
    }

    fn add_route_with_target(
        &mut self,
        input: FrameworkResolvedRouteInput<'_>,
    ) -> Result<(), ExtractError> {
        let Some(path) = safe_route_value(input.path, input.command) else {
            return Ok(());
        };
        let method = if input.command {
            "CMD".to_owned()
        } else {
            input.method.to_ascii_uppercase()
        };
        let name = if input.command {
            format!("cmd {path}")
        } else {
            format!("{method} {path}")
        };
        let identity = format!("{}::{path}", method.to_ascii_lowercase());
        let search_text = if input.command {
            format!("cli command {path}")
        } else {
            format!("route {method} {path}")
        };
        self.add_landmark(LandmarkInput {
            kind: SymbolKind::Route,
            name,
            identity,
            start: input.start,
            end: input.end,
            body_search_text: search_text,
            target: input.target,
        })
    }

    pub(crate) fn add_landmark(&mut self, input: LandmarkInput<'_>) -> Result<(), ExtractError> {
        self.add_landmark_with_id(input).map(|_| ())
    }

    /// Attach a safe framework label to the landmark just admitted, without
    /// searching existing symbols or changing earlier declarations.
    pub(crate) fn add_signed_landmark(
        &mut self,
        input: LandmarkInput<'_>,
        signature: String,
    ) -> Result<Option<SymbolId>, ExtractError> {
        if looks_sensitive(&signature) || signature.len() > MAX_SIGNAL_BYTES {
            return Ok(None);
        }
        let id = self.add_landmark_with_id(input)?;
        if let Some(id) = &id {
            self.bridge.budget.reserve_fact(
                u64::try_from(signature.len()).map_err(|_| ExtractError::OutputLimit)?,
                [signature.as_str()],
            )?;
            if let Some(symbol) = self.bridge.file.symbols.last_mut()
                && &symbol.id == id
            {
                symbol.signature = Some(signature);
            }
        }
        Ok(id)
    }

    /// Mark a landmark added by this pass as declaring, not defining, its target.
    pub(crate) fn mark_landmark_declaration_only(&mut self, id: &SymbolId) {
        if let Some(symbol) = self.bridge.file.symbols[self.bridge.original_symbols..]
            .iter_mut()
            .rev()
            .find(|symbol| &symbol.id == id)
        {
            symbol.implementation.declaration_only = true;
        }
    }

    pub(crate) fn add_landmark_with_id(
        &mut self,
        input: LandmarkInput<'_>,
    ) -> Result<Option<SymbolId>, ExtractError> {
        if landmark_is_rejected(self, &input) {
            return Ok(None);
        }
        let LandmarkInput {
            kind,
            name,
            identity,
            start,
            end,
            body_search_text,
            target,
        } = input;
        let Some(identity) = reserve_landmark_identity(self, kind, &identity) else {
            return Ok(None);
        };
        if !self.source().is_empty() {
            self.bridge.charge_work(1)?;
        }
        let owner = self.owner_near(start);
        let span = landmark_span(self, start, end)?;
        let id = framework_symbol_id(FrameworkSymbolIdentity {
            path: self.snapshot.path().as_str(),
            kind,
            qualified_name: &identity.qualified_name,
            ordinal: identity.ordinal,
        });
        let structural_digest = framework_digest(kind.as_str(), &identity.source_identity);
        let symbol = ExtractedSymbol {
            id: id.clone(),
            kind,
            name,
            qualified_name: identity.qualified_name,
            span,
            signature: None,
            docstring: None,
            body_search_text,
            body_search_truncated: false,
            health: crate::SymbolHealthMetrics::default(),
            implementation: SymbolImplementationFlags::default(),
            export: SymbolExportFlags::named(true),
            execution: SymbolExecutionFlags::default(),
            declaration_syntax: crate::DeclarationSyntax::Other,
            visibility: Some(Visibility::Public),
            clone_shape_digest: structural_digest.clone(),
            structural_digest,
            clone_token_profile: None,
        };
        self.bridge.budget.reserve_fact(
            symbol_budget_bytes(&symbol),
            [
                symbol.name.as_str(),
                symbol.qualified_name.as_str(),
                symbol.body_search_text.as_str(),
            ],
        )?;
        add_landmark_containment(self, owner, &id)?;
        let site_bytes = LANDMARK_SITE_INDEX_ENTRY_BYTES
            .saturating_add(u64::try_from(identity.source_identity.len()).unwrap_or(u64::MAX));
        self.bridge.budget.reserve_working_bytes(site_bytes)?;
        self.landmark_sites
            .insert((symbol.kind, identity.source_identity, start, end));
        self.bridge.file.symbols.push(symbol);
        if let Some((target, resolution_name, target_start, target_end)) = target {
            self.add_reference_with_resolution(FrameworkReferenceInput {
                owner: Some(id.clone()),
                name: target,
                resolution_name,
                kind: ReferenceKind::Calls,
                start: target_start,
                end: target_end,
            })?;
        }
        Ok(Some(id))
    }

    fn add_signal_reference(
        &mut self,
        input: FrameworkSignalReferenceInput<'_>,
    ) -> Result<(), ExtractError> {
        let owner = self.owner_near(input.start);
        self.add_reference(FrameworkReferenceInput {
            owner,
            name: input.name,
            resolution_name: None,
            kind: ReferenceKind::References,
            start: input.start,
            end: input.end,
        })
    }

    pub(crate) fn add_reference(
        &mut self,
        input: FrameworkReferenceInput<'_>,
    ) -> Result<(), ExtractError> {
        self.add_reference_with_resolution(input)
    }

    pub(crate) fn add_reference_with_resolution(
        &mut self,
        input: FrameworkReferenceInput<'_>,
    ) -> Result<(), ExtractError> {
        let Some(name) = safe_signal(input.name) else {
            return Ok(());
        };
        let resolution_name = input.resolution_name.and_then(safe_signal);
        let span = self.lines.span(SourceByteRange::new(
            input.start,
            input.end,
            self.source().len(),
        ))?;
        let key = (
            input.owner.clone(),
            input.kind,
            name.clone(),
            span.start_byte(),
            span.end_byte(),
        );
        if let Some(index) = self.reference_keys.get(&key).copied() {
            if let Some(resolution_name) = resolution_name {
                self.bridge.refine_reference(index, resolution_name)?;
            }
            return Ok(());
        }
        let resolution_name = crate::walk::non_jsx_resolution_name((
            self.language(),
            input.kind,
            resolution_name.as_deref().unwrap_or(&name),
        ))?
        .or(resolution_name);
        self.reference_keys
            .insert(key, self.bridge.file.references.len());
        let reference = ExtractedReference {
            owner: input.owner,
            name,
            resolution_name,
            kind: input.kind,
            span,
        };
        self.bridge.budget.reserve_fact(
            reference_budget_bytes(&reference),
            [
                reference.name.as_str(),
                reference.resolution_name.as_deref().unwrap_or(""),
            ],
        )?;
        self.bridge.file.references.push(reference);
        Ok(())
    }

    pub(crate) fn add_reference_near(
        &mut self,
        input: FrameworkNearReferenceInput<'_>,
    ) -> Result<(), ExtractError> {
        let owner = self.owner_near(input.start);
        self.add_reference(FrameworkReferenceInput {
            owner,
            name: input.name,
            resolution_name: input.resolution_name,
            kind: input.kind,
            start: input.start,
            end: input.end,
        })
    }

    pub(crate) fn add_reference_near_with_resolution(
        &mut self,
        input: FrameworkNearReferenceInput<'_>,
    ) -> Result<(), ExtractError> {
        self.add_reference_near(input)
    }

    fn owner_near(&self, offset: usize) -> Option<SymbolId> {
        // Every declaration of an empty convention file is at the same
        // synthetic site; the original nearest-owner rule selects the first.
        if self.source().is_empty() {
            return self.original_symbol(0).map(|symbol| symbol.id.clone());
        }
        let offset = u64::try_from(offset).ok()?;
        self.bridge
            .original_ownership
            .as_ref()?
            .near(offset)
            .and_then(|owner| self.original_symbol(owner))
            .map(|symbol| symbol.id.clone())
    }

    fn finish(self) -> Result<ExtractedFile, ExtractError> {
        if !self.bridge.retained_output_fits() {
            return Err(ExtractError::OutputLimit);
        }
        Ok(self.bridge.file)
    }
}

fn index_reference_sites(
    file: &ExtractedFile,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<BTreeMap<ReferenceSite, usize>, ExtractError> {
    let mut sites = BTreeMap::new();
    for (index, reference) in file.references.iter().enumerate() {
        if index.is_multiple_of(CANCELLATION_INTERVAL_FRAMEWORK_ITEMS) && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        sites
            .entry((
                reference.owner.clone(),
                reference.kind,
                reference.name.clone(),
                reference.span.start_byte(),
                reference.span.end_byte(),
            ))
            .or_insert(index);
    }
    Ok(sites)
}

fn scan_framework_signals(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    fwjs::file_routes::scan(builder)?;
    let hints = FrameworkHints::detect(builder.language(), builder.path(), source);
    scan_configuration_routes(builder, source)?;
    scan_codeigniter_routes(builder, source)?;
    fwjs::scan(builder, source)?;
    scan_swiftui_components(builder, source)?;
    scan_swiftui_app_entries(builder, source)?;
    scan_flutter_material_routes(builder, source)?;
    for (statement_start, statement) in StatementRanges::new(source) {
        builder.check_cancelled()?;
        if hints.routing.routes {
            scan_route_statement(
                builder,
                RouteStatementInput {
                    hints,
                    start: statement_start,
                    statement,
                },
            )?;
            scan_vapor_group_route(builder, statement_start, statement)?;
        }
        scan_cli_line(builder, statement_start, statement)?;
        scan_config_line(builder, statement_start, statement)?;
    }
    Ok(())
}

fn scan_swiftui_components(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Swift || !source.contains("SwiftUI") {
        return Ok(());
    }
    let candidates = builder.bridge.file.symbols[..builder.bridge.original_symbols]
        .iter()
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Class | SymbolKind::Struct))
        .filter_map(|symbol| {
            let marker = format!("struct {}", symbol.name);
            let declaration = source.find(&marker)?;
            let header_end = source[declaration..]
                .find('{')
                .map_or(source.len(), |offset| declaration + offset);
            let is_view = source[declaration..header_end]
                .split(':')
                .nth(1)
                .is_some_and(|inheritance| {
                    inheritance
                        .split(|character: char| !character.is_ascii_alphanumeric())
                        .any(|base| base == "View")
                });
            if !is_view {
                return None;
            }
            Some((
                symbol.name.clone(),
                usize::try_from(symbol.span.start_byte()).ok()?,
                usize::try_from(symbol.span.end_byte()).ok()?,
            ))
        })
        .collect::<Vec<_>>();
    for (name, start, end) in candidates {
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Component,
            name: name.clone(),
            identity: format!("swiftui-view::{name}"),
            start,
            end,
            body_search_text: format!("swiftui view component {name}"),
            target: None,
        })?;
    }
    Ok(())
}

/// The attribute marking a Swift program's entry type.
const SWIFT_MAIN_ATTRIBUTE: &str = "@main";
/// The `SwiftUI` protocol an app entry conforms to.
const SWIFTUI_APP_PROTOCOL: &str = "App";
/// The keyword that introduces a struct declaration.
const SWIFT_STRUCT_KEYWORD: &str = "struct";
/// The keyword that starts a generic constraint clause after the base list.
const SWIFT_WHERE_KEYWORD: &str = "where";

/// The `SwiftUI` app entry (`@main struct ShopApp: App`) is a class landmark
/// over its struct, as in v1's `SwiftUI` resolver.
fn scan_swiftui_app_entries(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Swift || !source.contains(SWIFT_MAIN_ATTRIBUTE) {
        return Ok(());
    }
    let entries = builder.bridge.file.symbols[..builder.bridge.original_symbols]
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Struct)
        .filter_map(|symbol| {
            let start = usize::try_from(symbol.span.start_byte()).ok()?;
            let end = usize::try_from(symbol.span.end_byte()).ok()?;
            is_swiftui_app_entry(source.get(start..end)?).then(|| (symbol.name.clone(), start, end))
        })
        .collect::<Vec<_>>();
    for (name, start, end) in entries {
        let landmark = builder.add_landmark_with_id(LandmarkInput {
            kind: SymbolKind::Class,
            identity: format!("swiftui-app::{name}"),
            body_search_text: format!("swiftui app entry {name}"),
            name,
            start,
            end,
            target: None,
        })?;
        // The struct itself defines the type; the landmark only marks it as
        // the app entry, so it never competes with the struct in resolution.
        if let Some(id) = landmark {
            builder.mark_landmark_declaration_only(&id);
        }
    }
    Ok(())
}

/// Whether a struct declaration is attributed `@main` and lists `App` among
/// its base types, judged on its comment- and literal-free header. Only bases
/// outside angle brackets count: `Gen<T: App>` bounds a parameter and
/// `Entry<App>` passes an argument, and neither conforms to `App`.
fn is_swiftui_app_entry(declaration: &str) -> bool {
    let header = swift_header_code(declaration);
    let Some(keyword) = find_word(&header, SWIFT_STRUCT_KEYWORD) else {
        return false;
    };
    let main = header[..keyword]
        .split_whitespace()
        .any(|token| token == SWIFT_MAIN_ATTRIBUTE);
    let mut depth = 0_usize;
    let top_level: String = header[keyword..]
        .chars()
        .map(|character| {
            let outside = depth == 0;
            match character {
                '<' => depth = depth.saturating_add(1),
                '>' => depth = depth.saturating_sub(1),
                _ => {}
            }
            if outside && character != '<' {
                character
            } else {
                ' '
            }
        })
        .collect();
    let Some((_, inheritance)) = top_level.split_once(':') else {
        return false;
    };
    main && inheritance
        .split(|character: char| !character.is_ascii_alphanumeric())
        .take_while(|word| *word != SWIFT_WHERE_KEYWORD)
        .any(|base| base == SWIFTUI_APP_PROTOCOL)
}

/// Byte offset of the first standalone occurrence of `word` in `text`.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let is_identifier = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(word)
        .map(|(index, _)| index)
        .find(|&index| {
            let before = text[..index].chars().next_back();
            let after = text[index + word.len()..].chars().next();
            !before.is_some_and(is_identifier) && !after.is_some_and(is_identifier)
        })
}

/// A Swift declaration's code before its body's `{`, with line and (nested)
/// block comments removed and string literals emptied.
fn swift_header_code(declaration: &str) -> String {
    let mut code = String::with_capacity(declaration.len());
    let mut characters = declaration.chars().peekable();
    let mut block_depth = 0_usize;
    while let Some(character) = characters.next() {
        let next = characters.peek().copied();
        if block_depth > 0 {
            match (character, next) {
                ('*', Some('/')) => {
                    characters.next();
                    block_depth -= 1;
                    code.push(' ');
                }
                ('/', Some('*')) => {
                    characters.next();
                    block_depth = block_depth.saturating_add(1);
                }
                _ => {}
            }
            continue;
        }
        match (character, next) {
            ('/', Some('*')) => {
                characters.next();
                block_depth = 1;
            }
            ('/', Some('/')) => {
                characters.by_ref().find(|skipped| *skipped == '\n');
                code.push('\n');
            }
            ('"', _) => {
                skip_string_literal(&mut characters);
                code.push_str("\"\"");
            }
            ('{', _) => break,
            _ => code.push(character),
        }
    }
    code
}

/// Consume a string literal's contents through its closing quote, honoring
/// backslash escapes.
fn skip_string_literal(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                characters.next();
            }
            '"' => return,
            _ => {}
        }
    }
}

fn scan_flutter_material_routes(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Dart || !source.contains("MaterialApp") {
        return Ok(());
    }
    let Some(routes) = source.find("routes:") else {
        return Ok(());
    };
    let Some(open) = source[routes..].find('{').map(|offset| routes + offset) else {
        return Ok(());
    };
    let Some(close) = matching_delimiter(DelimiterInput::braces(source, open)) else {
        return Ok(());
    };
    let mut cursor = open + 1;
    while cursor < close {
        let Some(path) = quoted_after(source, cursor) else {
            break;
        };
        if path.start >= close {
            break;
        }
        let after = path.end.saturating_add(1);
        let colon = source[after..close].find(':').map(|offset| after + offset);
        let Some(colon) = colon else {
            break;
        };
        let next_comma = source[colon..close]
            .find(',')
            .map_or(close, |offset| colon + offset);
        let handler = source[colon..next_comma].find("=>").and_then(|arrow| {
            let handler_start = colon + arrow + 2;
            identifiers(&source[handler_start..next_comma])
                .into_iter()
                .find(|(_, name)| !matches!(*name, "const" | "new"))
                .map(|(offset, name)| {
                    (
                        name,
                        handler_start + offset,
                        handler_start + offset + name.len(),
                    )
                })
        });
        builder.add_route(FrameworkRouteInput {
            method: "ANY",
            path: path.value,
            start: path.start,
            end: path.end,
            command: false,
            handler,
        })?;
        cursor = next_comma.saturating_add(1);
    }
    Ok(())
}

fn scan_vapor_group_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    statement_start: usize,
    statement: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Swift {
        return Ok(());
    }
    let Some(grouped) = statement.find(".grouped(") else {
        return Ok(());
    };
    let Some(prefix) = quoted_after(statement, grouped + ".grouped(".len()) else {
        return Ok(());
    };
    for (marker, method) in [
        (".get(", "GET"),
        (".post(", "POST"),
        (".put(", "PUT"),
        (".patch(", "PATCH"),
        (".delete(", "DELETE"),
    ] {
        let Some(method_start) = statement[prefix.end..]
            .find(marker)
            .map(|offset| prefix.end + offset + marker.len())
        else {
            continue;
        };
        let Some(path) = quoted_after(statement, method_start) else {
            continue;
        };
        let combined = format!(
            "/{}/{}",
            prefix.value.trim_matches('/'),
            path.value.trim_matches('/')
        );
        let handler = handler_after(statement, path.end).map(|(start, name)| {
            (
                name,
                statement_start + start,
                statement_start + start + name.len(),
            )
        });
        builder.add_route(FrameworkRouteInput {
            method,
            path: &combined,
            start: statement_start + path.start,
            end: statement_start + path.end,
            command: false,
            handler,
        })?;
    }
    Ok(())
}

pub(crate) fn matching_delimiter(input: DelimiterInput<'_>) -> Option<usize> {
    if input.value.as_bytes().get(input.open) != Some(&input.opening) {
        return None;
    }
    let bytes = input.value.as_bytes();
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes
        .iter()
        .copied()
        .enumerate()
        .take(input.limit)
        .skip(input.open)
    {
        if consume_quoted_byte(byte, &mut quote, &mut escaped) {
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
        } else if byte == input.opening {
            depth = depth.saturating_add(1);
        } else if byte == input.closing {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

pub(crate) fn join_route_paths(base: &str, subpath: &str) -> Option<String> {
    if [base, subpath]
        .into_iter()
        .any(crate::walk::specifier_safety::specifier_may_carry_credential)
    {
        return None;
    }
    if base.len().saturating_add(subpath.len()) > MAX_ROUTE_BYTES {
        return None;
    }
    let mut path = String::new();
    path.try_reserve(base.len().saturating_add(subpath.len()).saturating_add(1))
        .ok()?;
    for segment in base.split('/').chain(subpath.split('/')) {
        let segment = segment.trim();
        if !segment.is_empty() {
            path.push('/');
            path.push_str(segment);
        }
    }
    if path.is_empty() {
        path.push('/');
    }
    Some(path)
}

#[derive(Clone, Copy)]
struct FrameworkHints {
    routing: RoutingHints,
    ecosystem: EcosystemHints,
}

#[derive(Clone, Copy)]
struct RoutingHints {
    routes: bool,
    play_routes: bool,
}

#[derive(Clone, Copy)]
struct EcosystemHints {
    angular_or_flutter: bool,
    hono_only: bool,
}

impl FrameworkHints {
    fn detect(language: SourceLanguage, path: &str, source: &str) -> Self {
        Self {
            routing: RoutingHints {
                routes: framework_route_hint(language, path, source),
                play_routes: is_play_route_path(path),
            },
            ecosystem: EcosystemHints {
                angular_or_flutter: angular_or_flutter_hint(language, source),
                hono_only: (source.contains("new Hono") || source.contains("new OpenAPIHono"))
                    && !source.contains("express"),
            },
        }
    }
}

fn contains_any(source: &str, hints: &[&str]) -> bool {
    hints.iter().any(|hint| source.contains(hint))
}

fn framework_route_hint(language: SourceLanguage, path: &str, source: &str) -> bool {
    let hints: &[&str] = match language {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => return javascript_framework_route_hint(source),
        SourceLanguage::Python => &["django", "flask", "fastapi", "APIRouter", "urlpatterns"],
        SourceLanguage::Php => &["Route::", "Symfony", "$routes", "CodeIgniter", "Drupal"],
        SourceLanguage::Ruby => &["Rails.application.routes", "resources ", "namespace "],
        SourceLanguage::Java
        | SourceLanguage::Kotlin
        | SourceLanguage::Scala
        | SourceLanguage::CSharp => &[
            "Mapping(",
            "@RequestMapping",
            "[Route(",
            "[HttpGet",
            "[HttpPost",
            "MapGet(",
            "MapPost(",
        ],
        SourceLanguage::Go => &["gin.", "echo.", "chi.", "net/http", "HandleFunc("],
        SourceLanguage::Rust => &["actix_web", "axum", "rocket", "warp::"],
        SourceLanguage::Dart => &["GoRoute(", "MaterialApp("],
        SourceLanguage::Swift => &["Vapor", "routes"],
        SourceLanguage::Yaml => return is_play_route_path(path),
        _ => &[],
    };
    contains_any(source, hints)
}

fn javascript_framework_route_hint(source: &str) -> bool {
    [
        "express",
        "hono",
        "fastify",
        "Router",
        "Routes",
        "RouterModule",
    ]
    .iter()
    .any(|hint| contains_identifier_token(source, hint))
        || contains_any(source, &["Bun.serve", "@nestjs"])
}

fn contains_identifier_token(source: &str, token: &str) -> bool {
    if token.is_empty() || source.len() < token.len() {
        return false;
    }
    source
        .as_bytes()
        .windows(token.len())
        .enumerate()
        .any(|(start, candidate)| {
            candidate == token.as_bytes()
                && start
                    .checked_sub(1)
                    .and_then(|index| source.as_bytes().get(index))
                    .is_none_or(|byte| !is_framework_identifier_byte(*byte))
                && source
                    .as_bytes()
                    .get(start.saturating_add(token.len()))
                    .is_none_or(|byte| !is_framework_identifier_byte(*byte))
        })
}

const fn is_framework_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn angular_or_flutter_hint(language: SourceLanguage, source: &str) -> bool {
    let supported = matches!(
        language,
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::Dart
    );
    supported
        && source.contains("path:")
        && contains_any(
            source,
            &["Routes", "RouterModule", "GoRoute", "MaterialApp"],
        )
}

fn scan_route_statement(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteStatementInput<'_>,
) -> Result<(), ExtractError> {
    let RouteStatementInput {
        hints,
        start: statement_start,
        statement,
    } = input;
    if hints.routing.play_routes
        && let Some(route) = play_route(statement)
    {
        return builder.add_route(FrameworkRouteInput {
            method: route.method,
            path: route.path,
            start: statement_start + route.path_start,
            end: statement_start + route.path_start + route.path.len(),
            command: false,
            handler: route.handler.map(|(handler, start)| {
                (
                    handler,
                    statement_start + start,
                    statement_start + start + handler.len(),
                )
            }),
        });
    }
    if let Some(route) = annotation_route(statement) {
        builder.add_route(FrameworkRouteInput {
            method: route.method,
            path: route.path,
            start: statement_start + route.path_start,
            end: statement_start + route.path_start + route.path.len(),
            command: false,
            handler: None,
        })?;
    }
    scan_standard_route_markers(builder, input)?;
    if !hints.ecosystem.hono_only {
        scan_on_route(builder, statement_start, statement)?;
        scan_object_route(builder, statement_start, statement)?;
        scan_router_route(builder, statement_start, statement)?;
    }
    scan_resource_route(builder, statement_start, statement)?;
    if hints.ecosystem.angular_or_flutter
        && let Some(route) = named_path_route(statement)
    {
        builder.add_route(FrameworkRouteInput {
            method: "ANY",
            path: route.path,
            start: statement_start + route.path_start,
            end: statement_start + route.path_start + route.path.len(),
            command: false,
            handler: route.handler.map(|(handler, start)| {
                (
                    handler,
                    statement_start + start,
                    statement_start + start + handler.len(),
                )
            }),
        })?;
    }
    Ok(())
}

fn scan_standard_route_markers(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteStatementInput<'_>,
) -> Result<(), ExtractError> {
    for &marker in ROUTE_MARKERS {
        if input.hints.ecosystem.hono_only && HONO_GENERIC_ROUTE_MARKERS.contains(&marker.0) {
            continue;
        }
        scan_standard_route_marker(builder, input, marker)?;
    }
    Ok(())
}

fn scan_standard_route_marker(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteStatementInput<'_>,
    marker: (&str, &str, bool),
) -> Result<(), ExtractError> {
    let (marker, method, slash_required) = marker;
    let mut cursor = 0;
    while let Some(relative) = input.statement[cursor..].find(marker) {
        let marker_start = cursor + relative;
        let after_marker = marker_start + marker.len();
        if !marker_has_identifier_boundary(input.statement, marker_start, marker) {
            cursor = after_marker;
            continue;
        }
        if marker == ".all(" && route_marker_receiver(input.statement, marker_start) == "Promise" {
            cursor = after_marker;
            continue;
        }
        let Some(quoted) = quoted_after(input.statement, after_marker) else {
            cursor = after_marker;
            continue;
        };
        if slash_required && !quoted.value.starts_with('/') {
            cursor = quoted.end;
            continue;
        }
        let framework_target =
            if builder.language() == SourceLanguage::Php && marker.starts_with("Route::") {
                laravel_route_target(input.statement, quoted.end)
            } else {
                handler_after(input.statement, quoted.end).map(|(start, name)| RouteTarget {
                    name,
                    resolution_name: None,
                    start,
                    end: start + name.len(),
                })
            };
        builder.add_route_with_target(FrameworkResolvedRouteInput {
            method,
            path: quoted.value,
            start: input.start + quoted.start,
            end: input.start + quoted.end,
            command: false,
            target: framework_target.as_ref().map(|target| {
                (
                    target.name,
                    target.resolution_name.as_deref(),
                    input.start + target.start,
                    input.start + target.end,
                )
            }),
        })?;
        cursor = quoted.end;
    }
    Ok(())
}

fn route_marker_receiver(statement: &str, marker_start: usize) -> &str {
    let prefix = statement.get(..marker_start).unwrap_or_default();
    let start = prefix
        .rfind(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '$'))
        })
        .map_or(0, |index| index.saturating_add(1));
    prefix.get(start..).unwrap_or_default()
}

struct RouteMatch<'source> {
    method: &'source str,
    path: &'source str,
    path_start: usize,
    handler: Option<(&'source str, usize)>,
}

struct RouteTarget<'source> {
    name: &'source str,
    resolution_name: Option<String>,
    start: usize,
    end: usize,
}

pub(crate) struct Quoted<'source> {
    pub(crate) value: &'source str,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) quote_end: usize,
}

pub(crate) fn quoted_after(value: &str, from: usize) -> Option<Quoted<'_>> {
    scan_quoted(value, from, QuoteMode::Route)
}

pub(crate) fn quoted_literal_after(value: &str, from: usize) -> Option<Quoted<'_>> {
    scan_quoted(value, from, QuoteMode::Literal)
}

impl Quoted<'_> {
    pub(crate) fn with_offset(mut self, offset: usize) -> Self {
        self.start = self.start.saturating_add(offset);
        self.end = self.end.saturating_add(offset);
        self.quote_end = self.quote_end.saturating_add(offset);
        self
    }
}

#[derive(Clone, Copy)]
enum QuoteMode {
    Route,
    Literal,
}

fn scan_quoted(value: &str, from: usize, mode: QuoteMode) -> Option<Quoted<'_>> {
    let bytes = value.as_bytes();
    let mut cursor = from;
    while cursor < bytes.len()
        && !matches!(bytes[cursor], b'\'' | b'"')
        && !(matches!(mode, QuoteMode::Route) && bytes[cursor] == b'`')
    {
        if matches!(mode, QuoteMode::Route) && bytes[cursor] == b';' {
            return None;
        }
        cursor += 1;
    }
    let quote = *bytes.get(cursor)?;
    let start = cursor + 1;
    cursor = start;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor += 2;
            continue;
        }
        if bytes[cursor] == quote {
            return Some(Quoted {
                value: &value[start..cursor],
                start,
                end: cursor,
                quote_end: cursor,
            });
        }
        cursor += 1;
    }
    None
}

fn handler_after(value: &str, from: usize) -> Option<(usize, &str)> {
    let comma = value[from..].find(',')? + from + 1;
    identifiers(&value[comma..])
        .into_iter()
        .filter(|(_, name)| {
            !matches!(
                *name,
                "async"
                    | "function"
                    | "class"
                    | "new"
                    | "request"
                    | "response"
                    | "req"
                    | "res"
                    | "context"
                    | "state"
                    | "use"
            )
        })
        .take(16)
        .last()
        .map(|(offset, name)| (comma + offset, name))
}

fn laravel_route_target(statement: &str, from: usize) -> Option<RouteTarget<'_>> {
    let argument_start = statement[from..].find(',')?.checked_add(from + 1)?;
    let first_non_whitespace = statement[argument_start..]
        .find(|character: char| !character.is_ascii_whitespace())?
        .checked_add(argument_start)?;
    let argument = &statement[first_non_whitespace..];
    if argument.starts_with("function")
        || argument.starts_with("fn")
        || argument.starts_with("static function")
        || argument.starts_with("static fn")
    {
        return None;
    }

    if let Some(quoted) = quoted_after(statement, first_non_whitespace)
        && quoted.start <= first_non_whitespace.saturating_add(2)
        && let Some(resolution_name) = php_controller_resolution(quoted.value)
    {
        return Some(RouteTarget {
            name: quoted.value,
            resolution_name: Some(resolution_name),
            start: quoted.start,
            end: quoted.end,
        });
    }

    let Some(class_marker) = argument
        .find("::class")
        .and_then(|offset| offset.checked_add(first_non_whitespace))
    else {
        let (offset, name) = identifiers(argument)
            .into_iter()
            .find(|(_, name)| !matches!(*name, "new" | "static"))?;
        let start = first_non_whitespace.checked_add(offset)?;
        return Some(RouteTarget {
            name,
            resolution_name: None,
            start,
            end: start.checked_add(name.len())?,
        });
    };
    let (class_start, class_name) = php_qualified_token_before(statement, class_marker)?;
    let after_class = class_marker.checked_add("::class".len())?;
    let array_callable = statement[first_non_whitespace..class_start].contains('[');
    if array_callable
        && let Some(method) = quoted_after(statement, after_class)
        && let Some(resolution_name) = php_method_resolution(class_name, method.value)
    {
        return Some(RouteTarget {
            name: method.value,
            resolution_name: Some(resolution_name),
            start: method.start,
            end: method.end,
        });
    }
    Some(RouteTarget {
        name: class_name,
        resolution_name: php_class_resolution(class_name),
        start: class_start,
        end: class_marker,
    })
}

fn php_qualified_token_before(value: &str, end: usize) -> Option<(usize, &str)> {
    let bytes = value.as_bytes();
    if end == 0 || end > bytes.len() {
        return None;
    }
    let mut start = end;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric() || matches!(bytes[start - 1], b'_' | b'\\'))
    {
        start -= 1;
    }
    let token = value.get(start..end)?;
    php_class_resolution(token).map(|_| (start, token))
}

pub(crate) fn php_controller_resolution(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some((class, method)) = value.rsplit_once("::") {
        return php_method_resolution(class, method);
    }
    if let Some((class, method)) = value.rsplit_once('@') {
        return php_method_resolution(class, method);
    }
    php_class_resolution(value)
}

fn php_method_resolution(class: &str, method: &str) -> Option<String> {
    let method = method.trim();
    if !php_identifier(method) {
        return None;
    }
    let class = php_class_resolution(class)?;
    Some(format!("{class}::{method}"))
}

fn php_class_resolution(value: &str) -> Option<String> {
    let value = value.trim();
    let global = value.starts_with('\\');
    let value = value.strip_prefix('\\').unwrap_or(value);
    if value.is_empty() || value.len() > MAX_SIGNAL_BYTES {
        return None;
    }
    let mut segments = value.split('\\');
    let first = segments.next()?;
    if !php_identifier(first) {
        return None;
    }
    let mut collected = vec![first];
    for segment in segments {
        if !php_identifier(segment) {
            return None;
        }
        collected.push(segment);
    }
    let class = collected.pop()?;
    if collected.is_empty() {
        return Some(if global {
            format!("\\{class}")
        } else {
            class.to_owned()
        });
    }
    Some(format!(
        "{}{}::{class}",
        if global { "\\" } else { "" },
        collected.join("\\")
    ))
}

fn php_identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|first| *first == b'_' || first.is_ascii_alphabetic())
        && value
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn marker_has_identifier_boundary(statement: &str, marker_start: usize, marker: &str) -> bool {
    let Some(first) = marker.chars().next() else {
        return false;
    };
    if !route_identifier_continue(first) || marker_start == 0 {
        return true;
    }
    statement[..marker_start]
        .chars()
        .next_back()
        .is_none_or(|previous| !route_identifier_continue(previous))
}

fn route_identifier_continue(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphanumeric()
}

fn scan_on_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    statement_start: usize,
    statement: &str,
) -> Result<(), ExtractError> {
    let mut cursor = 0;
    while let Some(relative) = statement[cursor..].find(".on(") {
        let marker = cursor + relative + ".on(".len();
        let Some(method) = quoted_after(statement, marker) else {
            cursor = marker;
            continue;
        };
        let Some(path) = quoted_after(statement, method.end + 1) else {
            cursor = method.end + 1;
            continue;
        };
        let handler = handler_after(statement, path.end).map(|(start, handler)| {
            (
                handler,
                statement_start + start,
                statement_start + start + handler.len(),
            )
        });
        builder.add_route(FrameworkRouteInput {
            method: method.value,
            path: path.value,
            start: statement_start + path.start,
            end: statement_start + path.end,
            command: false,
            handler,
        })?;
        cursor = path.end;
    }
    Ok(())
}

fn scan_object_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    statement_start: usize,
    statement: &str,
) -> Result<(), ExtractError> {
    let Some(marker) = statement.find(".route(") else {
        return Ok(());
    };
    let body = &statement[marker + ".route(".len()..];
    let method = keyed_quoted(body, "method");
    let path = keyed_quoted(body, "url").or_else(|| keyed_quoted(body, "path"));
    let Some(path) = path else {
        return Ok(());
    };
    let offset = marker + ".route(".len();
    let method = method.map_or("ANY", |method| method.value);
    let handler = keyed_identifier(body, "handler").map(|(start, handler)| {
        (
            handler,
            statement_start + offset + start,
            statement_start + offset + start + handler.len(),
        )
    });
    builder.add_route(FrameworkRouteInput {
        method,
        path: path.value,
        start: statement_start + offset + path.start,
        end: statement_start + offset + path.end,
        command: false,
        handler,
    })
}

fn scan_router_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    statement_start: usize,
    statement: &str,
) -> Result<(), ExtractError> {
    let mut cursor = 0;
    while let Some(relative) = statement[cursor..].find(".route(") {
        let marker = cursor + relative + ".route(".len();
        let Some(path) = quoted_after(statement, marker) else {
            cursor = marker;
            continue;
        };
        let Some(comma) = statement[path.end..]
            .find(',')
            .map(|offset| path.end + offset + 1)
        else {
            cursor = path.end;
            continue;
        };
        let Some((method_offset, method)) =
            identifiers(&statement[comma..])
                .into_iter()
                .find(|(_, name)| {
                    matches!(
                        name.to_ascii_lowercase().as_str(),
                        "get" | "post" | "put" | "patch" | "delete" | "options" | "head" | "any"
                    )
                })
        else {
            cursor = path.end;
            continue;
        };
        let method_start = comma + method_offset;
        let handler = statement[method_start + method.len()..]
            .find('(')
            .map(|open| method_start + method.len() + open + 1)
            .and_then(|start| {
                identifiers(&statement[start..])
                    .into_iter()
                    .find(|(_, name)| !matches!(*name, "async" | "move"))
                    .map(|(offset, name)| {
                        (
                            name,
                            statement_start + start + offset,
                            statement_start + start + offset + name.len(),
                        )
                    })
            });
        builder.add_route(FrameworkRouteInput {
            method,
            path: path.value,
            start: statement_start + path.start,
            end: statement_start + path.end,
            command: false,
            handler,
        })?;
        cursor = path.end;
    }
    Ok(())
}

fn scan_resource_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    statement_start: usize,
    statement: &str,
) -> Result<(), ExtractError> {
    for marker in ["Route::resource(", "Route::apiResource("] {
        let mut cursor = 0;
        while let Some(relative) = statement[cursor..].find(marker) {
            let start = cursor + relative + marker.len();
            let Some(resource) = quoted_after(statement, start) else {
                cursor = start;
                continue;
            };
            let handler = laravel_route_target(statement, resource.end);
            builder.add_landmark(LandmarkInput {
                kind: SymbolKind::Route,
                name: format!("resource:{}", resource.value),
                identity: format!("resource::{}", resource.value),
                start: statement_start + resource.start,
                end: statement_start + resource.end,
                body_search_text: format!("resource route {}", resource.value),
                target: handler.as_ref().map(|target| {
                    (
                        target.name,
                        target.resolution_name.as_deref(),
                        statement_start + target.start,
                        statement_start + target.end,
                    )
                }),
            })?;
            cursor = resource.end;
        }
    }
    Ok(())
}

struct ConfigurationRoute {
    key: String,
    path: String,
    path_start: usize,
    method: String,
    target: Option<(String, Option<String>, usize, usize)>,
    drupal: bool,
}

fn scan_configuration_routes(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Yaml {
        return Ok(());
    }
    if crate::framework_drupal::is_routing_path(builder.path()) {
        return Ok(());
    }
    let lower_path = builder.path().to_ascii_lowercase();
    if !(lower_path.ends_with("routes.yaml")
        || lower_path.ends_with("routes.yml")
        || lower_path.ends_with("routing.yaml")
        || lower_path.ends_with("routing.yml"))
    {
        return Ok(());
    }
    let mut route = None;
    for (line_start, line) in physical_lines(source) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indentation = line.len().saturating_sub(line.trim_start().len());
        if indentation == 0 && trimmed.ends_with(':') && !trimmed.starts_with(['-', '{', '[']) {
            flush_configuration_route(builder, route.take())?;
            route = Some(ConfigurationRoute {
                key: trimmed.trim_end_matches(':').to_owned(),
                path: String::new(),
                path_start: line_start,
                method: "ANY".to_owned(),
                target: None,
                drupal: false,
            });
            continue;
        }
        let Some(active) = route.as_mut() else {
            continue;
        };
        if let Some((value, relative)) = yaml_scalar(line, "path") {
            value.clone_into(&mut active.path);
            active.path_start = line_start + relative;
        }
        if let Some((value, relative)) =
            yaml_scalar(line, "controller").or_else(|| yaml_scalar(line, "_controller"))
        {
            active.drupal |= line.contains("_controller");
            active.target = Some((
                value.to_owned(),
                php_controller_resolution(value),
                line_start + relative,
                line_start + relative + value.len(),
            ));
        }
        if trimmed.starts_with("methods:")
            && let Some((_, method)) = identifiers(trimmed)
                .into_iter()
                .find(|(_, value)| !value.eq_ignore_ascii_case("methods"))
        {
            active.method = method.to_ascii_uppercase();
        }
    }
    flush_configuration_route(builder, route)
}

fn flush_configuration_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    route: Option<ConfigurationRoute>,
) -> Result<(), ExtractError> {
    let Some(route) = route.filter(|route| {
        !route.key.is_empty() && !route.path.is_empty() && route.path.len() <= MAX_ROUTE_BYTES
    }) else {
        return Ok(());
    };
    let name = if route.drupal {
        format!("{} {}", route.method, route.path)
    } else {
        route.key.clone()
    };
    let target = route
        .target
        .as_ref()
        .map(|(name, resolution_name, start, end)| {
            (name.as_str(), resolution_name.as_deref(), *start, *end)
        });
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Route,
        name,
        identity: format!(
            "config-route::{}::{}::{}",
            route.key, route.method, route.path
        ),
        start: route.path_start,
        end: route.path_start + route.path.len(),
        body_search_text: format!(
            "framework config route {} {} {}",
            route.key, route.method, route.path
        ),
        target,
    })
}

fn yaml_scalar<'line>(line: &'line str, key: &str) -> Option<(&'line str, usize)> {
    let trimmed = line.trim_start();
    let key_start = line.len().saturating_sub(trimmed.len());
    let suffix = trimmed.strip_prefix(key)?;
    let colon = suffix.find(':')?;
    let raw = suffix[colon + 1..].trim();
    if raw.is_empty() {
        return None;
    }
    let unquoted = raw
        .strip_circumfix('\'', '\'')
        .or_else(|| raw.strip_circumfix('"', '"'))
        .unwrap_or(raw);
    let relative = line
        .find(unquoted)
        .unwrap_or(key_start + key.len() + colon + 1);
    Some((unquoted, relative))
}

fn scan_codeigniter_routes(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Php || !source.contains("$route[") {
        return Ok(());
    }
    for (line_start, line) in physical_lines(source) {
        let Some(marker) = line.find("$route[") else {
            continue;
        };
        let Some(path) = quoted_after(line, marker + "$route[".len()) else {
            continue;
        };
        let Some(equals) = line[path.end..].find('=').map(|index| path.end + index + 1) else {
            continue;
        };
        let handler = quoted_after(line, equals);
        if path.value.eq_ignore_ascii_case("translate_uri_dashes") || handler.is_none() {
            continue;
        }
        let rendered_path = if path.value.eq_ignore_ascii_case("default_controller") {
            "/".to_owned()
        } else {
            format!("/{}", path.value.trim_start_matches('/'))
        };
        let method = quoted_after(line, path.end.saturating_add(1))
            .filter(|method| method.start < equals)
            .map_or("ANY", |method| method.value);
        let resolution = handler
            .as_ref()
            .and_then(|handler| codeigniter_route_resolution(handler.value));
        let target = handler.as_ref().map(|handler| {
            (
                handler.value,
                resolution.as_deref(),
                line_start + handler.start,
                line_start + handler.end,
            )
        });
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Route,
            name: format!("{} {rendered_path}", method.to_ascii_uppercase()),
            identity: format!(
                "codeigniter-route::{}::{rendered_path}",
                method.to_ascii_lowercase()
            ),
            start: line_start + path.start,
            end: line_start + path.end,
            body_search_text: format!("codeigniter route {method} {rendered_path}"),
            target,
        })?;
    }
    Ok(())
}

fn codeigniter_route_resolution(handler: &str) -> Option<String> {
    let parts = handler
        .split('/')
        .filter(|part| !part.is_empty() && !part.starts_with('$'))
        .collect::<Vec<_>>();
    let controller_index = parts.len().saturating_sub(2);
    let controller = *parts.get(controller_index)?;
    let method = parts.get(controller_index + 1).copied().unwrap_or("index");
    let mut class = controller.to_owned();
    if let Some(first) = class.as_bytes().first() {
        class.replace_range(..1, &char::from(first.to_ascii_uppercase()).to_string());
    }
    Some(format!("{class}::{method}"))
}

fn keyed_quoted<'source>(value: &'source str, key: &str) -> Option<Quoted<'source>> {
    let marker = value.find(key)? + key.len();
    let colon = value[marker..].find(':')? + marker + 1;
    quoted_after(value, colon)
}

fn keyed_identifier<'source>(value: &'source str, key: &str) -> Option<(usize, &'source str)> {
    let marker = value.find(key)? + key.len();
    let colon = value[marker..].find(':')? + marker + 1;
    let (offset, name) = identifiers(&value[colon..]).into_iter().next()?;
    Some((colon + offset, name))
}

fn annotation_route(line: &str) -> Option<RouteMatch<'_>> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let (marker, method) = [
        ("#[get", "GET"),
        ("#[post", "POST"),
        ("#[put", "PUT"),
        ("#[patch", "PATCH"),
        ("#[delete", "DELETE"),
        ("@app.get", "GET"),
        ("@app.post", "POST"),
        ("@app.put", "PUT"),
        ("@app.patch", "PATCH"),
        ("@app.delete", "DELETE"),
        ("@app.route", "ANY"),
        ("@router.get", "GET"),
        ("@router.post", "POST"),
        ("@router.put", "PUT"),
        ("@router.patch", "PATCH"),
        ("@router.delete", "DELETE"),
    ]
    .into_iter()
    .find(|(marker, _)| trimmed.starts_with(marker))?;
    let quoted = quoted_after(trimmed, marker.len())?;
    Some(RouteMatch {
        method,
        path: quoted.value,
        path_start: indent + quoted.start,
        handler: None,
    })
}

fn named_path_route(line: &str) -> Option<RouteMatch<'_>> {
    let marker = line.find("path:")?;
    let quoted = quoted_after(line, marker + "path:".len())?;
    let handler = ["component:", "builder:", "pageBuilder:"]
        .into_iter()
        .find_map(|key| {
            let start = line.find(key)? + key.len();
            let (offset, name) = identifiers(&line[start..])
                .into_iter()
                .find(|(_, name)| !matches!(*name, "context" | "state" | "const" | "new"))?;
            Some((name, start + offset))
        });
    Some(RouteMatch {
        method: "ANY",
        path: quoted.value,
        path_start: quoted.start,
        handler,
    })
}

fn play_route(line: &str) -> Option<RouteMatch<'_>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let indent = line.find(trimmed)?;
    let mut parts = trimmed.split_ascii_whitespace();
    let method = parts.next()?;
    if !matches_ignore_ascii_case(
        method,
        &["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"],
    ) {
        return None;
    }
    let path = parts.next()?;
    // A handler may declare its parameters (`Users.show(id: Long)`) or an empty
    // list (`Users.create()`); the action is the text before the list.
    let handler = parts
        .next()
        .map(|handler| {
            handler
                .split_once('(')
                .map_or(handler, |(action, _)| action)
        })
        .filter(|action| !action.is_empty());
    let path_start = indent + trimmed.find(path)?;
    Some(RouteMatch {
        method,
        path,
        path_start,
        handler: handler.map(|handler| (handler, indent + trimmed.find(handler).unwrap_or(0))),
    })
}

fn scan_cli_line(
    builder: &mut FrameworkBuilder<'_, '_>,
    line_start: usize,
    line: &str,
) -> Result<(), ExtractError> {
    for marker in ["Command::new("] {
        if builder.language() != SourceLanguage::Rust {
            continue;
        }
        let Some(marker_start) = line.find(marker) else {
            continue;
        };
        let Some(command) = quoted_after(line, marker_start + marker.len()) else {
            continue;
        };
        builder.add_route(FrameworkRouteInput {
            method: "CMD",
            path: command.value,
            start: line_start + command.start,
            end: line_start + command.end,
            command: true,
            handler: None,
        })?;
    }
    if builder.language() == SourceLanguage::Go
        && builder.source().contains("cobra.Command")
        && let Some(marker) = line.find("Use:")
        && let Some(command) = quoted_after(line, marker + "Use:".len())
    {
        builder.add_route(FrameworkRouteInput {
            method: "CMD",
            path: command
                .value
                .split_ascii_whitespace()
                .next()
                .unwrap_or(command.value),
            start: line_start + command.start,
            end: line_start + command.end,
            command: true,
            handler: None,
        })?;
    }
    Ok(())
}

fn scan_config_line(
    builder: &mut FrameworkBuilder<'_, '_>,
    line_start: usize,
    line: &str,
) -> Result<(), ExtractError> {
    let input = ConfigSourceLine {
        start: line_start,
        text: line,
    };
    scan_dotted_config_references(builder, input)?;
    scan_quoted_config_references(
        builder,
        input,
        &[
            "process.env[",
            "Bun.env[",
            "c.env[",
            "ctx.env[",
            "context.env[",
            "os.environ[",
            "ENV[",
            "$_ENV[",
        ],
    )?;
    scan_quoted_config_references(
        builder,
        input,
        &[
            "Deno.env.get(",
            "os.getenv(",
            "os.environ.get(",
            "getenv(",
            "os.Getenv(",
            "os.LookupEnv(",
            "System.getenv(",
            "Environment.GetEnvironmentVariable(",
            "std::env::var(",
            "std::env::var_os(",
            "env::var(",
            "env::var_os(",
            "env!(",
            "ENV.fetch(",
            "env(",
            "config(",
            "getProperty(",
        ],
    )?;
    scan_runtime_path_references(builder, input)?;
    scan_managed_config_placeholders(builder, input)
}

#[derive(Clone, Copy)]
struct ConfigSourceLine<'source> {
    start: usize,
    text: &'source str,
}

fn scan_dotted_config_references(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ConfigSourceLine<'_>,
) -> Result<(), ExtractError> {
    for marker in [
        "process.env.",
        "Bun.env.",
        "import.meta.env.",
        "c.env.",
        "ctx.env.",
        "context.env.",
    ] {
        let mut cursor = 0;
        while let Some(relative) = input.text[cursor..].find(marker) {
            let start = cursor + relative + marker.len();
            let Some((offset, name)) = identifiers(&input.text[start..]).into_iter().next() else {
                break;
            };
            let name_start = start + offset;
            builder.add_signal_reference(FrameworkSignalReferenceInput {
                name,
                start: input.start + name_start,
                end: input.start + name_start + name.len(),
            })?;
            cursor = name_start + name.len();
        }
    }
    Ok(())
}

fn scan_quoted_config_references(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ConfigSourceLine<'_>,
    markers: &[&str],
) -> Result<(), ExtractError> {
    for marker in markers {
        let mut cursor = 0;
        while let Some(relative) = input.text[cursor..].find(marker) {
            let marker_start = cursor + relative;
            let start = marker_start + marker.len();
            if !marker_has_identifier_boundary(input.text, marker_start, marker) {
                cursor = start;
                continue;
            }
            let Some(value) = quoted_after(input.text, start) else {
                cursor = start;
                continue;
            };
            builder.add_signal_reference(FrameworkSignalReferenceInput {
                name: value.value,
                start: input.start + value.start,
                end: input.start + value.end,
            })?;
            cursor = value.end;
        }
    }
    Ok(())
}

fn scan_runtime_path_references(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ConfigSourceLine<'_>,
) -> Result<(), ExtractError> {
    for marker in [
        "__dirname",
        "__filename",
        "import.meta.dirname",
        "import.meta.filename",
        "import.meta.url",
    ] {
        let mut cursor = 0;
        while let Some(relative) = input.text[cursor..].find(marker) {
            let start = cursor + relative;
            let end = start + marker.len();
            let boundary_before = start == 0
                || (!input.text.as_bytes()[start.saturating_sub(1)].is_ascii_alphanumeric()
                    && input.text.as_bytes()[start.saturating_sub(1)] != b'_');
            let boundary_after = end == input.text.len()
                || (!input.text.as_bytes()[end].is_ascii_alphanumeric()
                    && input.text.as_bytes()[end] != b'_');
            if boundary_before && boundary_after {
                builder.add_signal_reference(FrameworkSignalReferenceInput {
                    name: marker,
                    start: input.start + start,
                    end: input.start + end,
                })?;
            }
            cursor = end;
        }
    }
    Ok(())
}

fn scan_managed_config_placeholders(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ConfigSourceLine<'_>,
) -> Result<(), ExtractError> {
    if matches!(
        builder.language(),
        SourceLanguage::Java | SourceLanguage::Kotlin | SourceLanguage::Scala
    ) {
        let mut cursor = 0;
        while let Some(relative) = input.text[cursor..].find("${") {
            let open = cursor + relative;
            let start = open + 2;
            let Some(close_relative) = input.text[start..].find('}') else {
                break;
            };
            let close = start + close_relative;
            let raw = input.text[start..close]
                .split(':')
                .next()
                .unwrap_or_default()
                .trim();
            let leading = input.text[start..close].find(raw).unwrap_or(0);
            let key_start = input.start + start + leading;
            // `@Value("${key}") private int ttl;` belongs to `ttl`, whose span
            // does not cover its annotations.
            let mut owners = crate::framework_spring::value_annotation_owners(builder, key_start)
                .into_iter()
                .map(Some)
                .collect::<Vec<_>>();
            if owners.is_empty() {
                owners.push(builder.owner_near(key_start));
            }
            for owner in owners {
                builder.add_reference(FrameworkReferenceInput {
                    owner,
                    name: raw,
                    resolution_name: None,
                    kind: ReferenceKind::References,
                    start: key_start,
                    end: key_start + raw.len(),
                })?;
            }
            cursor = close + 1;
        }
    }
    Ok(())
}

pub(crate) fn safe_route_value(value: &str, command: bool) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_ROUTE_BYTES
        || looks_sensitive(value)
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return None;
    }
    if !command && value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let value = if command {
        value.split_ascii_whitespace().next().unwrap_or(value)
    } else {
        value
    };
    Some(value.to_owned())
}

fn safe_signal(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_SIGNAL_BYTES
        || looks_sensitive(value)
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return None;
    }
    if !value.bytes().all(valid_signal_byte) {
        return None;
    }
    Some(value.to_owned())
}

fn valid_signal_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/' | b'#' | b'$' | b'@')
        || byte == b'\\'
}

fn looks_sensitive(value: &str) -> bool {
    if crate::walk::specifier_safety::specifier_may_carry_credential(value) {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    [
        "sk_live_",
        "sk_test_",
        "ghp_",
        "github_pat_",
        "xoxb_",
        "xoxp_",
        "akia",
        "asia",
    ]
    .into_iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn framework_symbol_id(input: FrameworkSymbolIdentity<'_>) -> SymbolId {
    let mut hasher = blake3::Hasher::new_derive_key(FRAMEWORK_SYMBOL_DOMAIN);
    for field in [
        input.path.as_bytes(),
        input.kind.as_str().as_bytes(),
        input.qualified_name.as_bytes(),
    ] {
        hasher.update(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(field);
    }
    hasher.update(&input.ordinal.to_le_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    SymbolId::from_uuid_v8(bytes)
}

fn framework_digest(method: &str, path: &str) -> ContentDigest {
    let mut hasher = blake3::Hasher::new_derive_key(FRAMEWORK_DIGEST_DOMAIN);
    for value in [method, path] {
        hasher.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn identifiers(value: &str) -> Vec<(usize, &str)> {
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !(bytes[cursor] == b'_' || bytes[cursor].is_ascii_alphabetic()) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len()
            && (bytes[cursor] == b'_'
                || bytes[cursor] == b'$'
                || bytes[cursor].is_ascii_alphanumeric())
        {
            cursor += 1;
        }
        output.push((start, &value[start..cursor]));
    }
    output
}

struct StatementRanges<'source> {
    source: &'source str,
    cursor: usize,
    start: usize,
    parentheses: usize,
    brackets: usize,
    quote: Option<u8>,
    escaped: bool,
}

impl<'source> StatementRanges<'source> {
    const fn new(source: &'source str) -> Self {
        Self {
            source,
            cursor: 0,
            start: 0,
            parentheses: 0,
            brackets: 0,
            quote: None,
            escaped: false,
        }
    }

    fn update_depth(&mut self, byte: u8) -> bool {
        match byte {
            b'(' => self.parentheses = self.parentheses.saturating_add(1),
            b')' => self.parentheses = self.parentheses.saturating_sub(1),
            b'[' => self.brackets = self.brackets.saturating_add(1),
            b']' => self.brackets = self.brackets.saturating_sub(1),
            _ => return false,
        }
        true
    }

    const fn is_boundary(&self, byte: u8) -> bool {
        matches!(byte, b'\n' | b';') && self.parentheses == 0 && self.brackets == 0
    }
}

impl<'source> Iterator for StatementRanges<'source> {
    type Item = (usize, &'source str);

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.source.as_bytes();
        while self.cursor < bytes.len() {
            let byte = bytes[self.cursor];
            self.cursor += 1;
            if consume_quote_state(byte, &mut self.quote, &mut self.escaped) {
                continue;
            }
            if self.update_depth(byte) {
                continue;
            }
            if self.is_boundary(byte) {
                let start = self.start;
                let end = self.cursor;
                self.start = self.cursor;
                return Some((start, &self.source[start..end]));
            }
        }
        if self.start < bytes.len() {
            let start = self.start;
            self.start = bytes.len();
            Some((start, &self.source[start..]))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
struct CommentSyntax {
    line: LineCommentSyntax,
    html: bool,
}

#[derive(Clone, Copy)]
struct LineCommentSyntax {
    python: bool,
    hash: bool,
    dash: bool,
}

#[derive(Clone, Copy)]
enum CommentKind {
    Line(usize),
    Block,
    Html,
}

fn mask_comments(
    source: &str,
    language: SourceLanguage,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<String, ExtractError> {
    if language == SourceLanguage::ObjectiveC {
        return mask_objc_comments(source, cancelled);
    }
    let bytes = source.as_bytes();
    let mut masked = Vec::new();
    masked
        .try_reserve_exact(bytes.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    masked.extend_from_slice(bytes);
    let syntax = comment_syntax(language);
    let mut cursor = 0;
    let mut quote = None;
    let mut escaped = false;
    while cursor < bytes.len() {
        if cursor % 4_096 == 0 && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if consume_quoted_byte(bytes[cursor], &mut quote, &mut escaped) {
            cursor += 1;
            continue;
        }
        if let Some(end) = python_multiline_end(bytes, cursor, syntax.line.python) {
            mask_comment_bytes(&mut masked, cursor, end);
            cursor = end;
            continue;
        }
        if matches!(bytes[cursor], b'\'' | b'"' | b'`') {
            quote = Some(bytes[cursor]);
            cursor += 1;
            continue;
        }
        let Some(kind) = comment_kind_at(bytes, cursor, syntax) else {
            cursor += 1;
            continue;
        };
        let end = comment_end(bytes, cursor, kind);
        mask_comment_bytes(&mut masked, cursor, end);
        cursor = end;
    }
    String::from_utf8(masked).map_err(|_| ExtractError::InvalidSpan)
}

fn mask_objc_comments(
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<String, ExtractError> {
    use crate::objc_lex::{ObjcLexer, Region};

    const CANCELLATION_INTERVAL_BYTES: usize = 4_096;
    let bytes = source.as_bytes();
    let mut masked = Vec::new();
    masked
        .try_reserve_exact(bytes.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    masked.extend_from_slice(bytes);
    let mut lexer = ObjcLexer::default();
    let mut cursor = 0;
    let mut next_probe = 0;
    while cursor < bytes.len() {
        if cursor >= next_probe {
            if cancelled() {
                return Err(ExtractError::Cancelled);
            }
            next_probe = cursor.saturating_add(CANCELLATION_INTERVAL_BYTES);
        }
        let (region, next) = lexer.step(bytes, cursor);
        if region == Region::Comment {
            mask_comment_bytes(&mut masked, cursor, next);
        }
        cursor = next;
    }
    String::from_utf8(masked).map_err(|_| ExtractError::InvalidSpan)
}

fn comment_syntax(language: SourceLanguage) -> CommentSyntax {
    CommentSyntax {
        line: LineCommentSyntax {
            python: language == SourceLanguage::Python,
            hash: matches!(
                language,
                SourceLanguage::Python
                    | SourceLanguage::Ruby
                    | SourceLanguage::Yaml
                    | SourceLanguage::Bash
                    | SourceLanguage::Zsh
                    | SourceLanguage::Fish
                    | SourceLanguage::Elixir
                    | SourceLanguage::R
                    | SourceLanguage::Php
            ),
            dash: matches!(
                language,
                SourceLanguage::Sql | SourceLanguage::Haskell | SourceLanguage::Lua
            ),
        },
        html: matches!(
            language,
            SourceLanguage::Html
                | SourceLanguage::Vue
                | SourceLanguage::Svelte
                | SourceLanguage::Astro
                | SourceLanguage::Xml
                | SourceLanguage::Visualforce
                | SourceLanguage::Aura
        ),
    }
}

pub(crate) fn consume_quote_state(byte: u8, quote: &mut Option<u8>, escaped: &mut bool) -> bool {
    if consume_quoted_byte(byte, quote, escaped) {
        return true;
    }
    if matches!(byte, b'\'' | b'"' | b'`') {
        *quote = Some(byte);
        return true;
    }
    false
}

pub(crate) fn consume_quoted_byte(byte: u8, quote: &mut Option<u8>, escaped: &mut bool) -> bool {
    let Some(active_quote) = *quote else {
        return false;
    };
    if *escaped {
        *escaped = false;
    } else if byte == b'\\' {
        *escaped = true;
    } else if byte == active_quote {
        *quote = None;
    }
    true
}

fn python_multiline_end(bytes: &[u8], cursor: usize, enabled: bool) -> Option<usize> {
    if !enabled || !(bytes[cursor..].starts_with(b"\"\"\"") || bytes[cursor..].starts_with(b"'''"))
    {
        return None;
    }
    let delimiter = &bytes[cursor..cursor + PYTHON_MULTILINE_DELIMITER_BYTES];
    let mut end = cursor.saturating_add(PYTHON_MULTILINE_DELIMITER_BYTES);
    while end + 2 < bytes.len() && &bytes[end..end + PYTHON_MULTILINE_DELIMITER_BYTES] != delimiter
    {
        end += 1;
    }
    Some(
        end.saturating_add(PYTHON_MULTILINE_DELIMITER_BYTES)
            .min(bytes.len()),
    )
}

fn comment_kind_at(bytes: &[u8], cursor: usize, syntax: CommentSyntax) -> Option<CommentKind> {
    if bytes[cursor..].starts_with(b"//") {
        Some(CommentKind::Line(2))
    } else if bytes[cursor..].starts_with(b"/*") {
        Some(CommentKind::Block)
    } else if syntax.html && bytes[cursor..].starts_with(b"<!--") {
        Some(CommentKind::Html)
    } else if syntax.line.dash && bytes[cursor..].starts_with(b"--") {
        Some(CommentKind::Line(2))
    } else if syntax.line.hash && bytes[cursor] == b'#' && bytes.get(cursor + 1) != Some(&b'[') {
        Some(CommentKind::Line(1))
    } else {
        None
    }
}

fn comment_end(bytes: &[u8], cursor: usize, kind: CommentKind) -> usize {
    match kind {
        CommentKind::Line(opening) => {
            let mut end = cursor.saturating_add(opening);
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            end
        }
        CommentKind::Block => {
            let mut end = cursor.saturating_add(2);
            while end + 1 < bytes.len() && !bytes[end..].starts_with(b"*/") {
                end += 1;
            }
            end.saturating_add(2).min(bytes.len())
        }
        CommentKind::Html => {
            let mut end = cursor.saturating_add(4);
            while end + 2 < bytes.len() && !bytes[end..].starts_with(b"-->") {
                end += 1;
            }
            end.saturating_add(3).min(bytes.len())
        }
    }
}

fn mask_comment_bytes(masked: &mut [u8], start: usize, end: usize) {
    for byte in &mut masked[start..end] {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn is_play_route_path(path: &str) -> bool {
    path.eq_ignore_ascii_case("conf/routes")
        || (path.to_ascii_lowercase().starts_with("conf/")
            && path.to_ascii_lowercase().ends_with(".routes"))
}

#[cfg(test)]
mod bridge_rollback_tests {
    use super::*;
    use crate::{NativeExtractor, SourceLimits};

    #[test]
    fn bridge_rollback_restores_refined_native_references() -> Result<(), Box<dyn std::error::Error>>
    {
        let source = "function ordinary(m) { doThing(); m.run(); }";
        let limits = SourceLimits::new(source.len())?;
        let snapshot = SourceSnapshot::from_bytes("src/undo.js", source.as_bytes(), limits)?;
        let file = NativeExtractor::new(SourceLanguage::JavaScript)?.extract(&snapshot)?;
        let original = serde_json::to_value(&file.references)?;
        assert!(file.references.iter().any(|reference| reference.name == "doThing" && reference.resolution_name.is_none()));
        let mut cancelled = || false;
        let mut builder =
            FrameworkBuilder::new(FrameworkInput::new(&snapshot, file), &mut cancelled)?;
        let checkpoint = builder.bridge.checkpoint();
        let start = source.find("doThing").ok_or("missing native call")?;
        builder.add_reference_near_with_resolution(FrameworkNearReferenceInput {
            name: "doThing",
            resolution_name: Some("Camera::doThing"),
            kind: ReferenceKind::Calls,
            start,
            end: start + "doThing".len(),
        })?;
        assert!(
            builder
                .references()
                .iter()
                .any(|reference| reference.resolution_name.as_deref() == Some("Camera::doThing"))
        );
        builder.bridge.omit_facts(checkpoint);
        assert_eq!(serde_json::to_value(builder.references())?, original);
        Ok(())
    }
}
