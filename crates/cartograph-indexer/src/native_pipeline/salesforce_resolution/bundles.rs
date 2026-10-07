//! Bundle-local client methods and explicit server/component conventions.

use cartograph_extract::{SalesforceBundle, SalesforceBundleKind, salesforce_bundle};

use super::{
    ImportBindingKind, ImportResolution, ReferenceKind, ReferenceResolution, ResolutionCandidate,
    ResolutionIndex, ResolutionRequest, ResolvedTarget, SALESFORCE_CONTROLLER_MODULE,
    SalesforceBindingQuery, StageItemFailure, SymbolKind, apex_class, apex_method, identifier,
    select_candidate, target,
};

const CLIENT_PROVENANCE: &str = "framework-salesforce-client-action";
const SERVER_PROVENANCE: &str = "framework-salesforce-server-action";
const LWC_PROVENANCE: &str = "framework-salesforce-lwc-component";

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(bundle) = salesforce_bundle(request.file_path) else {
        return Ok(None);
    };
    let target = if request.kind == ReferenceKind::Calls {
        action(index, (bundle, request), cancelled)?
    } else if matches!(
        request.kind,
        ReferenceKind::References | ReferenceKind::Imports
    ) && matches!(
        bundle.kind,
        SalesforceBundleKind::LwcTemplate | SalesforceBundleKind::LwcScript
    ) {
        component(index, (bundle, request.name), cancelled)?
    } else {
        None
    };
    Ok(target.map(ReferenceResolution::resolved))
}

fn action<Cancel>(
    index: &ResolutionIndex,
    query: (SalesforceBundle<'_>, &ResolutionRequest<'_>),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (bundle, request) = query;
    if let Some(name) = request
        .name
        .strip_prefix(SALESFORCE_CONTROLLER_MODULE)
        .and_then(|name| name.strip_prefix("::"))
    {
        return server(index, (bundle, name), cancelled);
    }
    let path = match bundle.kind {
        SalesforceBundleKind::AuraMarkup if identifier(request.name) => {
            format!("{}/{}Controller.js", bundle.directory, bundle.name)
        }
        SalesforceBundleKind::AuraController
        | SalesforceBundleKind::AuraHelper
        | SalesforceBundleKind::AuraRenderer => {
            if !client_site(index, request) {
                return Ok(None);
            }
            let Some((receiver, name)) = request.name.split_once('.') else {
                return Ok(None);
            };
            let path = match receiver {
                "helper" => format!("{}/{}Helper.js", bundle.directory, bundle.name),
                "this" => request.file_path.to_owned(),
                _ => return Ok(None),
            };
            return named(index, (&path, name, SymbolKind::Method), cancelled)
                .map(|candidate| candidate.map(|candidate| target(candidate, CLIENT_PROVENANCE)));
        }
        _ => return Ok(None),
    };
    Ok(
        named(index, (&path, request.name, SymbolKind::Method), cancelled)?
            .map(|candidate| target(candidate, CLIENT_PROVENANCE)),
    )
}

fn client_site(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    index
        .salesforce
        .sites
        .get(request.file_id)
        .and_then(|sites| sites.get(&request.span))
        .and_then(Option::as_ref)
        .is_some_and(|site| {
            matches!(site.kind, super::MarkupSiteKind::Client) && site.name == request.name
        })
}

fn server<Cancel>(
    index: &ResolutionIndex,
    query: (SalesforceBundle<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (bundle, name) = query;
    let mut found = None;
    for extension in ["cmp", "app"] {
        let path = format!("{}/{}.{extension}", bundle.directory, bundle.name);
        let Some(files) = index.modules.exact.get(&path) else {
            continue;
        };
        let [file] = files.as_slice() else {
            return Ok(None);
        };
        let Some(method) = controller_method(index, (file, name), cancelled)? else {
            return Ok(None);
        };
        if found.is_some_and(|known: &ResolutionCandidate| known.symbol_id != method.symbol_id) {
            return Ok(None);
        }
        found = Some(method);
    }
    Ok(found.map(|candidate| target(candidate, SERVER_PROVENANCE)))
}

fn controller_method<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&super::FileId, &str),
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, name) = query;
    let Some(Some(controllers)) = index.salesforce.controllers.get(file) else {
        return Ok(None);
    };
    let mut found = None;
    for controller in controllers {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(class) = apex_class(index, controller, cancelled)? else {
            return Ok(None);
        };
        let Some(method) = apex_method(index, (class, name), cancelled)?.candidate() else {
            return Ok(None);
        };
        if found.is_some_and(|known: &ResolutionCandidate| known.symbol_id != method.symbol_id) {
            return Ok(None);
        }
        found = Some(method);
    }
    Ok(found)
}

fn component<Cancel>(
    index: &ResolutionIndex,
    query: (SalesforceBundle<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (bundle, specifier) = query;
    let Some(name) = specifier.strip_prefix("c/").filter(|name| identifier(name)) else {
        return Ok(None);
    };
    let Some((root, _)) = bundle.directory.rsplit_once('/') else {
        return Ok(None);
    };
    let Some(path) = component_path(index, (root, name)) else {
        return Ok(None);
    };
    Ok(
        named(index, (&path, name, SymbolKind::Component), cancelled)?
            .map(|candidate| target(candidate, LWC_PROVENANCE)),
    )
}

fn component_path(index: &ResolutionIndex, query: (&str, &str)) -> Option<String> {
    let (root, name) = query;
    let mut found = None;
    for extension in ["js", "ts"] {
        let path = format!("{root}/{name}/{name}.{extension}");
        if let Some(files) = index.modules.exact.get(&path) {
            if files.len() != 1 {
                return None;
            }
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    found
}

fn named<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&str, &str, SymbolKind),
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (path, name, kind) = query;
    let key = format!("{path}::{name}");
    let Some(candidates) = index.candidates.get(&key) else {
        return Ok(None);
    };
    select_candidate(
        candidates.iter(),
        |candidate| candidate.kind == kind && candidate.qualified_name == key,
        cancelled,
    )
}

pub(super) fn resolve_binding<Cancel>(
    query: SalesforceBindingQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ImportResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if query.binding.kind != ImportBindingKind::Default {
        return Ok(None);
    }
    let Some(bundle) = salesforce_bundle(query.reference.file_path)
        .filter(|bundle| bundle.kind == SalesforceBundleKind::LwcScript)
    else {
        return Ok(None);
    };
    if ![
        query.binding.local_name.as_str(),
        query.binding.module_specifier.as_str(),
    ]
    .contains(&query.reference.name)
    {
        return Ok(None);
    }
    Ok(component(
        query.index,
        (bundle, &query.binding.module_specifier),
        cancelled,
    )?
    .map(ImportResolution::Resolved))
}
