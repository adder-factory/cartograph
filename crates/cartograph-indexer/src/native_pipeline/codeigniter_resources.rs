//! `CodeIgniter` resource conventions retain heuristic confidence and scope.

mod paths;

use super::{
    ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionRequest,
    ResolvedTarget, StageItemFailure, SymbolKind, Visibility, select_candidate, try_clone_text,
};

const INFERRED_PREFIX: &str = "ci-inferred::";
const INFERRED_CONFIDENCE: f32 = 0.70;
const ROUTE_CONFIDENCE: f32 = 0.85;
const CLASS_FALLBACK_CONFIDENCE: f32 = 0.70;
const LOADED_PREFIX: &str = "ci-loaded::";
const LOADED_CONFIDENCE: f32 = 0.90;

pub(super) fn typed_lookup(name: &str) -> bool {
    name.starts_with(INFERRED_PREFIX)
        || name.starts_with(LOADED_PREFIX)
        || name.starts_with("ci-route-root::")
        || name.starts_with("ci-route-path::")
}

struct Query<'a> {
    application: &'a str,
    class: &'a str,
    member: Option<&'a str>,
    directory: &'a str,
    resource_path: &'a str,
    inferred: bool,
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.name.starts_with("ci-route-path::") {
        return paths::resolve(index, request, cancelled);
    }
    let Some(query) = query(request) else {
        return Ok(None);
    };
    let Some(classes) = index.candidates.get(query.class) else {
        return Ok(None);
    };
    let class = select_candidate(
        classes.iter(),
        |candidate| {
            candidate.kind == SymbolKind::Class
                && candidate.qualified_name == query.class
                && in_directory(index, candidate, &query)
        },
        cancelled,
    )?;
    let Some(class) = class else {
        return Ok(None);
    };
    if query.member.is_none() {
        return Ok(Some(ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: class.symbol_id.clone(),
            kind: class.kind,
            confidence: LOADED_CONFIDENCE,
            provenance: "framework-codeigniter-loaded-resource-class",
        })));
    }
    resolve_member(index, (class, &query), cancelled)
}

fn resolve_member<Cancel>(
    index: &ResolutionIndex,
    input: (&ResolutionCandidate, &Query<'_>),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (class, query) = input;
    let Some(member) = query.member else {
        return Ok(None);
    };
    let mut key = try_clone_text(&class.qualified_name)?;
    key.try_reserve("::".len() + member.len())
        .map_err(|_| StageItemFailure)?;
    key.push_str("::");
    key.push_str(member);
    let candidates = index.candidates.get(&key);
    let Some(candidates) = candidates else {
        return Ok(class_fallback(class, query));
    };
    let candidate = select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.parent_symbol_id.as_ref() == Some(&class.symbol_id)
                && candidate.kind == SymbolKind::Method
        },
        cancelled,
    )?;
    Ok(candidate
        .filter(|candidate| matches!(candidate.visibility, None | Some(Visibility::Public)))
        .map(|candidate| {
            ReferenceResolution::resolved(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence: if query.inferred {
                    INFERRED_CONFIDENCE
                } else if query.directory != "application/controllers/" {
                    LOADED_CONFIDENCE
                } else {
                    ROUTE_CONFIDENCE
                },
                provenance: if query.inferred {
                    "framework-codeigniter-inferred-resource"
                } else if query.directory != "application/controllers/" {
                    "framework-codeigniter-loaded-resource"
                } else {
                    "framework-codeigniter-route-target"
                },
            })
        }))
}

fn query<'a>(request: &'a ResolutionRequest<'_>) -> Option<Query<'a>> {
    if request.language != "php"
        || !matches!(
            request.kind,
            ReferenceKind::Calls | ReferenceKind::References
        )
    {
        return None;
    }
    if let Some(resource) = request
        .name
        .strip_prefix(INFERRED_PREFIX)
        .or_else(|| request.name.strip_prefix(LOADED_PREFIX))
    {
        let (kind, resource) = resource.split_once("::")?;
        let (path, member) = resource
            .split_once("::")
            .map_or((resource, None), |(path, member)| (path, Some(member)));
        if (request.kind == ReferenceKind::Calls) != member.is_some() {
            return None;
        }
        let class = path.rsplit('/').next()?;
        let directory = match kind {
            "model" => "application/models/",
            "library" => "application/libraries/",
            _ => return None,
        };
        return Some(Query {
            application: owning_application(request.file_path)?,
            class,
            member,
            directory,
            resource_path: path,
            inferred: request.name.starts_with(INFERRED_PREFIX),
        });
    }
    if request.kind != ReferenceKind::Calls
        || !request.file_path.ends_with("application/config/routes.php")
    {
        return None;
    }
    let (class, member) = request
        .name
        .strip_prefix("ci-route-root::")?
        .split_once("::")?;
    Some(Query {
        application: request
            .file_path
            .strip_suffix("application/config/routes.php")?,
        class,
        member: Some(member),
        directory: "application/controllers/",
        resource_path: class,
        inferred: false,
    })
}

fn owning_application(path: &str) -> Option<&str> {
    let (application, _) = path.rsplit_once("application/")?;
    (application.is_empty() || application.ends_with('/')).then_some(application)
}

fn in_directory(
    index: &ResolutionIndex,
    candidate: &ResolutionCandidate,
    query: &Query<'_>,
) -> bool {
    index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|file| {
            if file.language != "php" {
                return false;
            }
            file.path
                == format!(
                    "{}{}{}.php",
                    query.application, query.directory, query.resource_path
                )
        })
}

fn class_fallback(class: &ResolutionCandidate, query: &Query<'_>) -> Option<ReferenceResolution> {
    (!query.inferred
        && query.directory == "application/controllers/"
        && query.member == Some("index"))
    .then(|| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: class.symbol_id.clone(),
            kind: class.kind,
            confidence: CLASS_FALLBACK_CONFIDENCE,
            provenance: "framework-codeigniter-controller-class-fallback",
        })
    })
}
