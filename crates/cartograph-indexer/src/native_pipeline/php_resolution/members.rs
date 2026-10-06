//! Bounded method lookup: class declaration, composed traits, then ancestors.
//! Adaptations, trait conflicts, missing types and excessive fan-out abstain.

use super::{
    ExactQuery, Intent, MAX_ANCESTRY_HOPS, MAX_TRAIT_VISITS, ResolutionCandidate, StageItemFailure,
    SymbolKind, Visibility, synthesized_key,
};

pub(super) enum MemberMatch<'index> {
    Missing,
    Unique(&'index ResolutionCandidate),
    Blocked,
}

#[derive(Clone, Copy)]
struct MemberQuery<'index, 'lookup> {
    exact: ExactQuery<'index, 'lookup>,
    carrier: &'index ResolutionCandidate,
    member: &'lookup str,
}

pub(super) fn resolve<'index, Cancel>(
    exact: ExactQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    Ok(match lookup(exact, cancelled)? {
        MemberMatch::Unique(candidate) => Some(candidate),
        _ => None,
    })
}

pub(super) fn lookup<'index, Cancel>(
    exact: ExactQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<MemberMatch<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(exact.intent, Intent::Member | Intent::DispatchMember)
        || exact.declared(cancelled)?
    {
        return Ok(MemberMatch::Blocked);
    }
    let Some((owner, member)) = exact.key.rsplit_once("::") else {
        return Ok(MemberMatch::Blocked);
    };
    let Some(mut carrier) = exact.class_like(owner, cancelled)? else {
        return Ok(MemberMatch::Blocked);
    };
    if !matches!(carrier.kind, SymbolKind::Class | SymbolKind::Enum) {
        return Ok(MemberMatch::Blocked);
    }
    for _ in 0..MAX_ANCESTRY_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let query = MemberQuery {
            exact,
            carrier,
            member,
        };
        match trait_member(query, cancelled)? {
            MemberMatch::Missing => {}
            found => return Ok(found),
        }
        match parent_member(query, cancelled)? {
            ParentStep::Ancestor(ancestor) => carrier = ancestor,
            ParentStep::Terminal(found) => return Ok(found),
        }
    }
    Ok(MemberMatch::Blocked)
}

enum ParentStep<'index> {
    Ancestor(&'index ResolutionCandidate),
    Terminal(MemberMatch<'index>),
}

fn parent_member<'index, Cancel>(
    query: MemberQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<ParentStep<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let parent = match query.exact.index.php.parents.get(&query.carrier.symbol_id) {
        None => return Ok(ParentStep::Terminal(MemberMatch::Missing)),
        Some(None) => return Ok(ParentStep::Terminal(MemberMatch::Blocked)),
        Some(Some(parent)) => parent,
    };
    let Some(ancestor) = query
        .exact
        .class_like(parent, cancelled)?
        .filter(|ancestor| ancestor.kind == SymbolKind::Class)
    else {
        return Ok(ParentStep::Terminal(MemberMatch::Blocked));
    };
    let Some(key) = synthesized_key(&[parent, "::", query.member]) else {
        return Ok(ParentStep::Terminal(MemberMatch::Blocked));
    };
    let inherited = ExactQuery {
        key: &key,
        ..query.exact
    };
    if inherited.declared(cancelled)? {
        let found = inherited
            .candidate(cancelled)?
            .map_or(MemberMatch::Blocked, MemberMatch::Unique);
        return Ok(ParentStep::Terminal(found));
    }
    Ok(ParentStep::Ancestor(ancestor))
}

fn trait_member<'index, Cancel>(
    query: MemberQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<MemberMatch<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut search = TraitSearch::new(query.carrier);
    while let Some(current) = search.next() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !search.visit((query, current), cancelled)? {
            return Ok(MemberMatch::Blocked);
        }
    }
    Ok(search
        .found
        .map_or(MemberMatch::Missing, MemberMatch::Unique))
}

struct TraitSearch<'index> {
    pending: [Option<&'index ResolutionCandidate>; MAX_TRAIT_VISITS],
    cursor: usize,
    length: usize,
    found: Option<&'index ResolutionCandidate>,
}

impl<'index> TraitSearch<'index> {
    fn new(carrier: &'index ResolutionCandidate) -> Self {
        let mut pending = [None; MAX_TRAIT_VISITS];
        pending[0] = Some(carrier);
        Self {
            pending,
            cursor: 0,
            length: 1,
            found: None,
        }
    }

    fn next(&mut self) -> Option<&'index ResolutionCandidate> {
        if self.cursor == self.length {
            return None;
        }
        let current = self.pending.get(self.cursor).copied().flatten();
        self.cursor += 1;
        current
    }

    fn visit<Cancel>(
        &mut self,
        input: (MemberQuery<'index, '_>, &ResolutionCandidate),
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (query, current) = input;
        let keys = match query.exact.index.php.composed.get(&current.symbol_id) {
            None => return Ok(true),
            Some(None) => return Ok(false),
            Some(Some(keys)) => keys,
        };
        for key in keys {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !self.apply(inspect_trait(query, key, cancelled)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn apply(&mut self, visit: TraitVisit<'index>) -> bool {
        match visit {
            TraitVisit::Skip => true,
            TraitVisit::Blocked => false,
            TraitVisit::Method(candidate) => self.retain(candidate),
            TraitVisit::Nested(trait_type) => {
                let Some(slot) = self.pending.get_mut(self.length) else {
                    return false;
                };
                *slot = Some(trait_type);
                self.length += 1;
                true
            }
        }
    }

    fn retain(&mut self, candidate: &'index ResolutionCandidate) -> bool {
        if self
            .found
            .is_some_and(|known| known.symbol_id != candidate.symbol_id)
        {
            return false;
        }
        self.found = Some(candidate);
        true
    }
}

#[derive(Clone, Copy)]
enum TraitVisit<'index> {
    Skip,
    Blocked,
    Method(&'index ResolutionCandidate),
    Nested(&'index ResolutionCandidate),
}

fn inspect_trait<'index, Cancel>(
    query: MemberQuery<'index, '_>,
    key: &str,
    cancelled: &mut Cancel,
) -> Result<TraitVisit<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    // These global PHP interfaces cannot supply a method implementation.
    if key.eq_ignore_ascii_case("JsonSerializable") || key.eq_ignore_ascii_case("Countable") {
        return Ok(TraitVisit::Skip);
    }
    let Some(composed) = query.exact.class_like(key, cancelled)? else {
        return Ok(TraitVisit::Blocked);
    };
    if composed.kind != SymbolKind::Trait {
        return Ok(TraitVisit::Skip);
    }
    let Some(member_key) = synthesized_key(&[key, "::", query.member]) else {
        return Ok(TraitVisit::Blocked);
    };
    let method = ExactQuery {
        key: &member_key,
        ..query.exact
    };
    if !method.declared(cancelled)? {
        return Ok(TraitVisit::Nested(composed));
    }
    let Some(candidate) = method
        .declaration(cancelled)?
        .filter(|candidate| !candidate.implementation.declaration_only)
    else {
        return Ok(TraitVisit::Blocked);
    };
    if !trait_visible(query, candidate, cancelled)? {
        return Ok(TraitVisit::Blocked);
    }
    Ok(TraitVisit::Method(candidate))
}

fn trait_visible<Cancel>(
    query: MemberQuery<'_, '_>,
    candidate: &ResolutionCandidate,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    match candidate.visibility {
        None | Some(Visibility::Public) => Ok(true),
        Some(Visibility::Internal) => Ok(false),
        Some(Visibility::Private | Visibility::Protected) => {
            let Some(caller) = query.exact.caller_class else {
                return Ok(false);
            };
            if caller == &query.carrier.symbol_id {
                return Ok(true);
            }
            Ok(candidate.visibility == Some(Visibility::Protected)
                && query
                    .exact
                    .descends_from((caller, &query.carrier.symbol_id), cancelled)?)
        }
    }
}
