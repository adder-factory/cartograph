//! Salesforce bindings over typed markup, import and annotation facts.

use std::collections::{HashMap, HashSet};

mod bundles;

use cartograph_extract::{
    SALESFORCE_CLIENT_MODULE, SALESFORCE_COMPONENT_MODULE, SALESFORCE_CONTROLLER_MODULE,
};

use super::{
    ExtractedImportBinding, FileId, ImportBindingKind, ImportResolution, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ReferenceResolution, ResolutionCandidate,
    ResolutionIndex, ResolutionRequest, ResolveBudget, ResolvedTarget, SourceSpan,
    StageItemFailure, SymbolId, SymbolKind, UNRESOLVED_PROVENANCE, Visibility,
    javascript_family_name, select_candidate, try_clone_text, usize_to_u64,
};

const MAX_CONTROLLERS: usize = 16;
const APEX_PROVENANCE: &str = "framework-salesforce-apex-method";

#[derive(Default)]
pub(super) struct SalesforceIndex {
    controllers: HashMap<FileId, Option<Vec<String>>>,
    aura_enabled: HashSet<SymbolId>,
    sites: HashMap<FileId, HashMap<SourceSpan, Option<MarkupSite>>>,
}

struct MarkupSite {
    kind: MarkupSiteKind,
    name: String,
}

#[derive(Clone, Copy)]
enum MarkupSiteKind {
    Controller,
    Component,
    Client,
}

pub(super) fn implicit_binding(binding: &ExtractedImportBinding, language: &str) -> bool {
    (matches!(language, "aura" | "visualforce") || javascript_family_name(language))
        && markup_site_kind(&binding.module_specifier).is_some()
}

fn markup_site_kind(module: &str) -> Option<MarkupSiteKind> {
    match module {
        SALESFORCE_CONTROLLER_MODULE => Some(MarkupSiteKind::Controller),
        SALESFORCE_COMPONENT_MODULE => Some(MarkupSiteKind::Component),
        SALESFORCE_CLIENT_MODULE => Some(MarkupSiteKind::Client),
        _ => None,
    }
}

pub(super) struct SalesforceFileInput<'index, 'file, 'budget> {
    pub(super) index: &'index mut SalesforceIndex,
    pub(super) file: &'file NativeFileFacts,
    pub(super) budget: &'budget mut ResolveBudget,
}

pub(super) fn index_file<Cancel>(
    input: SalesforceFileInput<'_, '_, '_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    match input.file.file.language.as_str() {
        "apex" => index_annotations(input, cancelled),
        "aura" | "visualforce" => index_controllers(input, cancelled),
        language if javascript_family_name(language) => index_controllers(input, cancelled),
        _ => Ok(()),
    }
}

fn index_annotations<Cancel>(
    input: SalesforceFileInput<'_, '_, '_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let SalesforceFileInput {
        index,
        file,
        budget,
    } = input;
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if reference.kind == ReferenceKind::Decorates
            && reference.name.eq_ignore_ascii_case("AuraEnabled")
            && let Some(owner) = &reference.owner
        {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    + usize_to_u64(size_of::<SymbolId>() + owner.as_str().len()),
            )?;
            index.aura_enabled.insert(owner.clone());
        }
    }
    Ok(())
}

fn index_controllers<Cancel>(
    input: SalesforceFileInput<'_, '_, '_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let SalesforceFileInput {
        index,
        file,
        budget,
    } = input;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        record_site(index, (&file.file.file_id, binding), budget)?;
        if binding.module_specifier == SALESFORCE_CONTROLLER_MODULE {
            record_controller(index, (&file.file.file_id, &binding.imported_name), budget)?;
        }
    }
    Ok(())
}

fn record_site(
    index: &mut SalesforceIndex,
    input: (&FileId, &ExtractedImportBinding),
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let (file_id, binding) = input;
    let Some(kind) = markup_site_kind(&binding.module_specifier) else {
        return Ok(());
    };
    if !index.sites.contains_key(file_id) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(
                    size_of::<(FileId, HashMap<SourceSpan, Option<MarkupSite>>)>()
                        + file_id.as_str().len(),
                ),
        )?;
        index.sites.insert(file_id.clone(), HashMap::new());
    }
    let sites = index.sites.get_mut(file_id).ok_or(StageItemFailure)?;
    if let Some(existing) = sites.get_mut(&binding.span) {
        *existing = None;
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(SourceSpan, Option<MarkupSite>)>() + binding.imported_name.len(),
            ),
    )?;
    sites.insert(
        binding.span,
        Some(MarkupSite {
            kind,
            name: try_clone_text(&binding.imported_name)?,
        }),
    );
    Ok(())
}

fn record_controller(
    index: &mut SalesforceIndex,
    controller: (&FileId, &str),
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let (file_id, name) = controller;
    if !index.controllers.contains_key(file_id) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(FileId, Option<Vec<String>>)>() + file_id.as_str().len()),
        )?;
        index.controllers.insert(file_id.clone(), Some(Vec::new()));
    }
    let names = index.controllers.get_mut(file_id).ok_or(StageItemFailure)?;
    let Some(known) = names else {
        return Ok(());
    };
    if known.iter().any(|known| known == name) {
        return Ok(());
    }
    if known.len() == MAX_CONTROLLERS {
        *names = None;
        return Ok(());
    }
    budget.charge(usize_to_u64(size_of::<String>() + name.len()))?;
    known.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
    known.push(try_clone_text(name)?);
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct SalesforceBindingQuery<'index, 'request> {
    pub(super) index: &'index ResolutionIndex,
    pub(super) binding: &'index ExtractedImportBinding,
    pub(super) reference: &'index ResolutionRequest<'request>,
}

pub(super) fn resolve_binding<Cancel>(
    query: SalesforceBindingQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ImportResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let SalesforceBindingQuery {
        index,
        binding,
        reference,
    } = query;
    if let Some(resolution) = bundles::resolve_binding(query, cancelled)? {
        return Ok(Some(resolution));
    }
    if !javascript_family_name(reference.language) || !apex_specifier(&binding.module_specifier) {
        return Ok(None);
    }
    if !matches!(binding.kind, ImportBindingKind::Default)
        || !matches!(
            reference.kind,
            ReferenceKind::Calls | ReferenceKind::References | ReferenceKind::Imports
        )
        || ![
            binding.local_name.as_str(),
            binding.module_specifier.as_str(),
        ]
        .contains(&reference.name)
    {
        return Ok(Some(ImportResolution::Unresolved));
    }
    let target = apex_import_target(index, &binding.module_specifier, cancelled)?;
    Ok(Some(target.map_or(
        ImportResolution::Unresolved,
        ImportResolution::Resolved,
    )))
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = bundles::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if javascript_family_name(request.language)
        && request.kind == ReferenceKind::Imports
        && apex_specifier(request.name)
    {
        return Ok(Some(resolution(apex_import_target(
            index,
            request.name,
            cancelled,
        )?)));
    }
    if !matches!(request.language, "aura" | "visualforce") {
        return Ok(None);
    }
    if request.kind == ReferenceKind::Calls {
        return Ok(Some(resolve_action(index, request, cancelled)?));
    }
    if request.kind == ReferenceKind::References {
        return resolve_markup_binding(index, request, cancelled);
    }
    Ok(None)
}

fn apex_specifier(name: &str) -> bool {
    name.starts_with("@salesforce/apex/") || name.starts_with("@salesforce/apexContinuation/")
}

fn apex_import_target<Cancel>(
    index: &ResolutionIndex,
    specifier: &str,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(raw) = specifier
        .strip_prefix("@salesforce/apex/")
        .or_else(|| specifier.strip_prefix("@salesforce/apexContinuation/"))
    else {
        return Ok(None);
    };
    let Some((class, method)) = raw.rsplit_once('.') else {
        return Ok(None);
    };
    // Indexed Apex declarations do not prove managed-package namespace membership.
    // An explicit prefix must never be discarded to reach a local class.
    if !identifier(method) || !identifier(class) {
        return Ok(None);
    }
    let Some(class) = apex_class(index, class, cancelled)? else {
        return Ok(None);
    };
    Ok(apex_method(index, (class, method), cancelled)?
        .candidate()
        .map(|candidate| target(candidate, APEX_PROVENANCE)))
}

fn apex_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    name: &str,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !identifier(name) {
        return Ok(None);
    }
    let Some(candidates) = index.candidates.get(name) else {
        return Ok(None);
    };
    select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.kind == SymbolKind::Class
                && candidate.qualified_name == name
                && apex_candidate(index, candidate)
        },
        cancelled,
    )
}

enum MethodMatch<'index> {
    Missing,
    Unique(&'index ResolutionCandidate),
    Ambiguous,
}

impl<'index> MethodMatch<'index> {
    fn candidate(self) -> Option<&'index ResolutionCandidate> {
        match self {
            Self::Unique(candidate) => Some(candidate),
            _ => None,
        }
    }
}

#[derive(Default)]
struct MethodSelection<'index> {
    best: Option<&'index ResolutionCandidate>,
    enabled: bool,
    count: usize,
}

impl<'index> MethodSelection<'index> {
    fn observe(&mut self, candidate: &'index ResolutionCandidate, annotated: bool) {
        if self.count == 0 || (annotated && !self.enabled) {
            self.best = Some(candidate);
            self.enabled = annotated;
            self.count = 1;
        } else if annotated == self.enabled {
            self.count = self.count.saturating_add(1);
        }
    }

    fn result(self) -> MethodMatch<'index> {
        match (self.count, self.best) {
            (0, _) => MethodMatch::Missing,
            (1, Some(candidate)) => MethodMatch::Unique(candidate),
            _ => MethodMatch::Ambiguous,
        }
    }
}

fn apex_method<'index, Cancel>(
    index: &'index ResolutionIndex,
    method: (&ResolutionCandidate, &str),
    cancelled: &mut Cancel,
) -> Result<MethodMatch<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (class, name) = method;
    let Some(key) = index
        .frameworks
        .framework_methods
        .key(&class.symbol_id, name)
    else {
        return Ok(MethodMatch::Missing);
    };
    let Some(candidates) = index.candidates.get(key) else {
        return Ok(MethodMatch::Missing);
    };
    let mut selection = MethodSelection::default();
    for candidate in candidates.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if eligible_method(index, (class, candidate)) {
            selection.observe(
                candidate,
                index
                    .frameworks
                    .salesforce
                    .aura_enabled
                    .contains(&candidate.symbol_id),
            );
        }
    }
    Ok(selection.result())
}

fn eligible_method(
    index: &ResolutionIndex,
    method: (&ResolutionCandidate, &ResolutionCandidate),
) -> bool {
    let (class, candidate) = method;
    candidate.kind == SymbolKind::Method
        && candidate.parent_symbol_id.as_ref() == Some(&class.symbol_id)
        && matches!(candidate.visibility, None | Some(Visibility::Public))
        && apex_candidate(index, candidate)
}

fn action_method<'index, Cancel>(
    index: &'index ResolutionIndex,
    method: (&ResolutionCandidate, &str),
    cancelled: &mut Cancel,
) -> Result<MethodMatch<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (class, name) = method;
    let Some(key) = index
        .frameworks
        .framework_methods
        .key(&class.symbol_id, name)
    else {
        return Ok(MethodMatch::Missing);
    };
    let Some(candidates) = index.candidates.get(key) else {
        return Ok(MethodMatch::Missing);
    };
    let candidate = select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.kind == SymbolKind::Method
                && candidate.parent_symbol_id.as_ref() == Some(&class.symbol_id)
        },
        cancelled,
    )?;
    Ok(candidate
        .filter(|candidate| eligible_method(index, (class, candidate)))
        .map_or(MethodMatch::Ambiguous, MethodMatch::Unique))
}

fn resolve_action<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language == "aura" {
        // Markup c.* actions name client-controller methods. Their extraction
        // is tracked separately; a server method with the same name is not proof.
        return Ok(resolution(None));
    }
    let Some(Some(controllers)) = index.frameworks.salesforce.controllers.get(request.file_id)
    else {
        return Ok(resolution(None));
    };
    let mut found: Option<&ResolutionCandidate> = None;
    for controller in controllers {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(class) = apex_class(index, controller, cancelled)? else {
            return Ok(resolution(None));
        };
        let method = match action_method(index, (class, request.name), cancelled)? {
            MethodMatch::Unique(method) => method,
            MethodMatch::Missing => continue,
            MethodMatch::Ambiguous => return Ok(resolution(None)),
        };
        if found.is_some_and(|known| known.symbol_id != method.symbol_id) {
            return Ok(resolution(None));
        }
        found = Some(method);
    }
    Ok(resolution(
        found.map(|candidate| target(candidate, APEX_PROVENANCE)),
    ))
}

fn resolve_markup_binding<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(site) = index
        .frameworks
        .salesforce
        .sites
        .get(request.file_id)
        .and_then(|sites| sites.get(&request.span))
    else {
        return Ok(None);
    };
    let Some(site) = site else {
        return Ok(Some(resolution(None)));
    };
    let target = match site.kind {
        MarkupSiteKind::Controller => apex_class(index, &site.name, cancelled)?
            .map(|candidate| target(candidate, "framework-salesforce-controller")),
        MarkupSiteKind::Component => component_target(index, (site, request), cancelled)?,
        MarkupSiteKind::Client => return Ok(None),
    };
    Ok(Some(resolution(target)))
}

fn component_target<Cancel>(
    index: &ResolutionIndex,
    site: (&MarkupSite, &ResolutionRequest<'_>),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (site, request) = site;
    let raw = site.name.as_str();
    let normalized = request.name;
    if let Some(candidates) = index.candidates.get(raw) {
        let candidate = select_candidate(
            candidates.iter(),
            |candidate| component_candidate(index, (candidate, request.language)),
            cancelled,
        )?;
        return Ok(candidate
            .map(|candidate| target(candidate, "framework-salesforce-component-convention")));
    }
    let Some(candidates) = index.candidates.get(normalized) else {
        return Ok(None);
    };
    Ok(select_candidate(
        candidates.iter(),
        |candidate| component_candidate(index, (candidate, request.language)),
        cancelled,
    )?
    .map(|candidate| target(candidate, "framework-salesforce-component-convention")))
}

fn component_candidate(index: &ResolutionIndex, component: (&ResolutionCandidate, &str)) -> bool {
    let (candidate, language) = component;
    candidate.kind == SymbolKind::Component
        && index
            .modules
            .files
            .get(&candidate.file_id)
            .is_some_and(|file| {
                file.language == language
                    && (language != "visualforce" || file.path.ends_with(".component"))
            })
}

fn apex_candidate(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|file| file.language == "apex")
}

fn identifier(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn target(candidate: &ResolutionCandidate, provenance: &'static str) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: 0.85,
        provenance,
    }
}

fn resolution(target: Option<ResolvedTarget>) -> ReferenceResolution {
    target.map_or_else(
        || ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE),
        ReferenceResolution::resolved,
    )
}
