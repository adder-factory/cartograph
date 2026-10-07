//! Literal CI route splits must agree with controller file directories.

use super::super::{
    ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionRequest,
    ResolvedTarget, StageItemFailure, SymbolKind, Visibility, select_candidate, try_clone_text,
};

const MAX_PATH_SEGMENTS: usize = 16;
const MAX_PATH_BYTES: usize = 1_024;
const CONTROLLER_DIRECTORY: &str = "application/controllers/";
const PATH_CONFIDENCE: f32 = 0.80;

struct Parts<'s> {
    values: [&'s str; MAX_PATH_SEGMENTS],
    len: usize,
}

enum Match<'a> {
    Absent,
    Blocked,
    Unique(&'a ResolutionCandidate),
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "php"
        || request.kind != ReferenceKind::Calls
        || !request.file_path.ends_with("application/config/routes.php")
    {
        return Ok(None);
    }
    let Some(parts) = request.name.strip_prefix("ci-route-path::").and_then(parts) else {
        return Ok(None);
    };
    let Some(application) = request
        .file_path
        .strip_suffix("application/config/routes.php")
    else {
        return Ok(None);
    };
    let mut matched: Option<&ResolutionCandidate> = None;
    for position in 0..parts.len.saturating_sub(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        match split_target(index, (&parts, position, application), cancelled)? {
            Match::Absent => {}
            Match::Blocked => return Ok(None),
            Match::Unique(candidate) => {
                if matched.is_some_and(|known| known.symbol_id != candidate.symbol_id) {
                    return Ok(None);
                }
                matched = Some(candidate);
            }
        }
    }
    Ok(matched.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: PATH_CONFIDENCE,
            provenance: "framework-codeigniter-route-path",
        })
    }))
}

fn parts(raw: &str) -> Option<Parts<'_>> {
    if raw.len() > MAX_PATH_BYTES {
        return None;
    }
    let mut result = Parts {
        values: [""; MAX_PATH_SEGMENTS],
        len: 0,
    };
    for value in raw.split('/') {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        *result.values.get_mut(result.len)? = value;
        result.len += 1;
    }
    Some(result)
}

fn split_target<'a, Cancel>(
    index: &'a ResolutionIndex,
    split: (&Parts<'_>, usize, &str),
    cancelled: &mut Cancel,
) -> Result<Match<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (parts, position, application) = split;
    let mut class = try_clone_text(parts.values[position])?;
    let first = class.as_bytes()[0].to_ascii_uppercase();
    class.replace_range(..1, &char::from(first).to_string());
    let Some(candidates) = index.candidates.get(&class) else {
        return Ok(Match::Absent);
    };
    let directory = parts.values[..position].join("/");
    let scope = (directory.as_str(), class.as_str(), application);
    let mut matched = None;
    for candidate in candidates.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind != SymbolKind::Class || !controller_path(index, candidate, scope) {
            continue;
        }
        if matched.is_some() {
            return Ok(Match::Blocked);
        }
        matched = Some(candidate);
    }
    let Some(class) = matched else {
        return Ok(Match::Absent);
    };
    method(index, (class, parts.values[position + 1]), cancelled)
}

fn controller_path(
    index: &ResolutionIndex,
    candidate: &ResolutionCandidate,
    scope: (&str, &str, &str),
) -> bool {
    let Some(file) = index
        .modules
        .files
        .get(&candidate.file_id)
        .filter(|file| file.language == "php")
    else {
        return false;
    };
    let relative = file
        .path
        .strip_prefix(scope.2)
        .and_then(|path| path.strip_prefix(CONTROLLER_DIRECTORY));
    let Some(relative) = relative else {
        return false;
    };
    let (directory, filename) = relative.rsplit_once('/').unwrap_or(("", relative));
    directory == scope.0
        && filename
            .strip_suffix(".php")
            .is_some_and(|name| name.eq_ignore_ascii_case(scope.1))
}

fn method<'a, Cancel>(
    index: &'a ResolutionIndex,
    member: (&ResolutionCandidate, &str),
    cancelled: &mut Cancel,
) -> Result<Match<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let key = format!("{}::{}", member.0.qualified_name, member.1);
    let Some(candidates) = index.candidates.get(&key) else {
        return Ok(Match::Absent);
    };
    let candidate = select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.parent_symbol_id.as_ref() == Some(&member.0.symbol_id)
                && candidate.kind == SymbolKind::Method
        },
        cancelled,
    )?;
    Ok(match candidate {
        Some(candidate) if matches!(candidate.visibility, None | Some(Visibility::Public)) => {
            Match::Unique(candidate)
        }
        _ => Match::Blocked,
    })
}
