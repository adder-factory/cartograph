//! Event dispatcher calls join resolved callable handlers with bounded fan-out.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    DerivedEdgeInput, EdgeKind, ResolutionCandidate, ResolutionIndex, ResolutionMutation,
    StageItemFailure, SymbolId, SymbolKind, append_derived_edge, framework_landmark_candidate,
    native_bridge_details, ordered_resolution_candidates,
};

mod resolved_handlers;
pub(super) use resolved_handlers::{ConsumerIndex, HandlerIndex, HandlerObservation, prepare};

pub(super) const PROVENANCE: &str = "framework-native-event-handler-call";
pub(super) const MAX_ENDPOINTS: usize = 6;
const ENTRY_BYTES: u64 = 128;
const MAX_OWNER_HOPS: usize = 64;
type Endpoints<'index> = BTreeMap<&'index str, BTreeSet<&'index SymbolId>>;
type Callables<'index> = BTreeMap<&'index SymbolId, &'index ResolutionCandidate>;

#[derive(Default)]
struct ChannelSites<'index> {
    consumers: BTreeMap<&'index SymbolId, &'index str>,
    producers: Vec<(&'index str, &'index ResolutionCandidate)>,
}

pub(super) fn resource_fanout_fits<Cancel>(
    candidates: &[ResolutionCandidate],
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut producers = 0;
    let mut consumers = 0;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        producers += usize::from(
            candidate
                .qualified_name
                .contains("::react-native-event-producer::"),
        );
        consumers += usize::from(
            candidate
                .qualified_name
                .contains("::react-native-event-consumer::"),
        );
        if producers > MAX_ENDPOINTS || consumers > MAX_ENDPOINTS {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn append<Cancel>(
    mut input: ResolutionMutation<'_, Cancel>,
    resolved: &HandlerIndex,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let sites = collect_sites(&mut input)?;
    if sites.producers.is_empty() || sites.consumers.is_empty() {
        return Ok(());
    }
    let callables = collect_callables(&mut input)?;
    let producers = dispatchers(&mut input, (&sites, &callables))?;
    let handlers = resolved_handlers(&mut input, (&sites.consumers, &callables), resolved)?;
    append_calls(input, (&producers, &handlers))
}

fn collect_sites<'index, Cancel>(
    input: &mut ResolutionMutation<'index, Cancel>,
) -> Result<ChannelSites<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut sites = ChannelSites::default();
    for (name, candidates) in ordered_resolution_candidates(input.index) {
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            if candidate
                .qualified_name
                .contains("::react-native-event-consumer::")
            {
                input.budget.charge(ENTRY_BYTES)?;
                sites.consumers.insert(&candidate.symbol_id, name);
            }
            if candidate
                .qualified_name
                .contains("::react-native-event-producer::")
            {
                input.budget.charge(ENTRY_BYTES)?;
                sites
                    .producers
                    .try_reserve(1)
                    .map_err(|_| StageItemFailure)?;
                sites.producers.push((name, candidate));
            }
        }
    }
    Ok(sites)
}

fn collect_callables<'index, Cancel>(
    input: &mut ResolutionMutation<'index, Cancel>,
) -> Result<Callables<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut callables = BTreeMap::new();
    for (name, candidates) in ordered_resolution_candidates(input.index) {
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            if matches!(candidate.kind, SymbolKind::Method | SymbolKind::Function)
                && !framework_landmark_candidate(candidate)
            {
                input.budget.charge(ENTRY_BYTES)?;
                callables.insert(&candidate.symbol_id, candidate);
            }
        }
    }
    Ok(callables)
}

fn dispatchers<'index, Cancel>(
    input: &mut ResolutionMutation<'index, Cancel>,
    (sites, callables): (&ChannelSites<'index>, &Callables<'index>),
) -> Result<Endpoints<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut producers = BTreeMap::<&str, BTreeSet<&SymbolId>>::new();
    for (name, site) in &sites.producers {
        if let Some(owner) = callable_owner(input.index, (site, callables), input.cancelled)? {
            input.budget.charge(ENTRY_BYTES)?;
            producers.entry(name).or_default().insert(owner);
        }
    }
    Ok(producers)
}

fn resolved_handlers<'index, Cancel>(
    input: &mut ResolutionMutation<'_, Cancel>,
    (consumers, callables): (&BTreeMap<&SymbolId, &'index str>, &Callables<'_>),
    resolved: &HandlerIndex,
) -> Result<BTreeMap<&'index str, BTreeSet<SymbolId>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut handlers = BTreeMap::<&str, BTreeSet<SymbolId>>::new();
    for (owner, targets) in &resolved.targets {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        let Some(event) = consumers.get(owner).copied() else {
            continue;
        };
        for target in targets {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            if callables.contains_key(target) {
                input.budget.charge(ENTRY_BYTES)?;
                handlers.entry(event).or_default().insert(target.clone());
            }
        }
    }
    Ok(handlers)
}

fn append_calls<Cancel>(
    mut input: ResolutionMutation<'_, Cancel>,
    (producers, handlers): (&Endpoints<'_>, &BTreeMap<&str, BTreeSet<SymbolId>>),
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for (event, sources) in producers {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        let Some(targets) = handlers.get(event) else {
            continue;
        };
        if sources.len() <= MAX_ENDPOINTS && targets.len() <= MAX_ENDPOINTS {
            append_channel(&mut input, (sources, targets))?;
        }
    }
    Ok(())
}

fn append_channel<Cancel>(
    input: &mut ResolutionMutation<'_, Cancel>,
    (sources, targets): (&BTreeSet<&SymbolId>, &BTreeSet<SymbolId>),
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for source in sources {
        for target in targets {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            append_derived_edge(
                input.facts,
                input.budget,
                DerivedEdgeInput {
                    source_symbol_id: source,
                    target_symbol_id: target,
                    kind: EdgeKind::Calls,
                    confidence: native_bridge_details::CONVENTION_CONFIDENCE,
                    provenance: PROVENANCE,
                },
            )?;
        }
    }
    Ok(())
}

fn callable_owner<'index, Cancel>(
    index: &'index ResolutionIndex,
    (site, callables): (&ResolutionCandidate, &Callables<'index>),
    cancelled: &mut Cancel,
) -> Result<Option<&'index SymbolId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut owner = &site.symbol_id;
    for _ in 0..MAX_OWNER_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(callable) = callables.get(owner) {
            return Ok((callable.file_id == site.file_id
                && callable.declaration_span.0 <= site.declaration_span.0
                && site.declaration_span.1 <= callable.declaration_span.1)
                .then_some(&callable.symbol_id));
        }
        let Some(parent) = index.parents.get(owner) else {
            return Ok(None);
        };
        owner = parent;
    }
    Ok(None)
}
