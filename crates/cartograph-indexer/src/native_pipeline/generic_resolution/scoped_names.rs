//! Candidates compete only within their owning scope, including fields and overloads.

use std::{collections::HashMap, mem::size_of};

use super::super::{RESOLUTION_MAP_NODE_ALLOWANCE, try_clone_text, usize_to_u64};
use super::{
    CandidateMap, ResolutionCandidate, ResolutionCandidateInsertion, ResolutionIndex,
    ResolutionRequest, ResolveBudget, StageItemFailure, SymbolId, casefold, push_candidate,
};

#[derive(Default)]
pub(super) struct ScopedNames {
    owners: HashMap<SymbolId, CandidateMap>,
    files: HashMap<u64, CandidateMap>,
}

fn scope_map<'a, Key: Clone + Eq + std::hash::Hash>(
    maps: &'a mut HashMap<Key, CandidateMap>,
    (key, key_bytes): (&Key, u64),
    budget: &mut ResolveBudget,
) -> Result<&'a mut CandidateMap, StageItemFailure> {
    if !maps.contains_key(key) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(Key, CandidateMap)>()))
                .saturating_add(key_bytes),
        )?;
        maps.try_reserve(1).map_err(|_| StageItemFailure)?;
        maps.insert(key.clone(), CandidateMap::new());
    }
    maps.get_mut(key).ok_or(StageItemFailure)
}

pub(super) fn insert(
    names: &mut ScopedNames,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let candidates = if let Some(owner) = insertion.parent_symbol_id {
        scope_map(
            &mut names.owners,
            (owner, usize_to_u64(owner.as_str().len())),
            budget,
        )?
    } else {
        scope_map(&mut names.files, (&insertion.file_ordinal, 0), budget)?
    };
    let mut name = try_clone_text(insertion.key)?;
    if casefold::case_insensitive_language(insertion.language) {
        name.make_ascii_lowercase();
    }
    push_candidate(
        candidates,
        ResolutionCandidateInsertion {
            key: &name,
            ..insertion
        },
        budget,
    )
}

pub(super) fn members<'a>(
    index: &'a ResolutionIndex,
    owner: &SymbolId,
    name: &str,
) -> &'a [ResolutionCandidate] {
    index
        .generic
        .scoped_names
        .owners
        .get(owner)
        .and_then(|names| names.get(name))
        .map_or(&[], super::super::ResolutionCandidateBucket::as_slice)
}

pub(super) fn candidates<'a>(
    index: &'a ResolutionIndex,
    request: &ResolutionRequest<'_>,
    scope: Option<&SymbolId>,
) -> Result<&'a [ResolutionCandidate], StageItemFailure> {
    let mut name = try_clone_text(request.name)?;
    if casefold::case_insensitive_language(request.language) {
        name.make_ascii_lowercase();
    }
    if let Some(owner) = scope {
        return Ok(members(index, owner, &name));
    }
    let ordinal = index
        .file_ordinals
        .get(request.file_id)
        .ok_or(StageItemFailure)?;
    Ok(index
        .generic
        .scoped_names
        .files
        .get(ordinal)
        .and_then(|names| names.get(&name))
        .map_or(&[], super::super::ResolutionCandidateBucket::as_slice))
}
