//! Physical bridge endpoints and bounded convention-derived relationships.

use std::collections::{BTreeSet, HashMap};

use super::{
    DYNAMIC_DISPATCH_RESOLUTION_PREFIX, ExtractedReference, FileResolutionContext,
    FrameworkEdgeInput, NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX, ReferenceKind, ReferenceResolution,
    ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget, ResolutionMutation, ResolveBudget,
    ResolvedTarget, StageItemFailure, SymbolId, SymbolKind, append_framework_edge,
    framework_landmark_candidate, javascript_family_name, javascript_member_resolution,
    native_bridge_target_language, ordered_resolution_candidates, tagged_framework_module,
    usize_to_u64,
};

pub(super) const PHYSICAL_METHOD_PROVENANCE: &str = "framework-native-physical-method";
pub(super) const CONVENTION_CONFIDENCE: f32 = 0.6;
// Candidate storage, map overhead and cloned stable IDs; text is charged below.
const MAP_ENTRY_BYTES: u64 = 512;
pub(super) const ALIAS_PROVENANCE: &str = "framework-native-module-alias";

pub(super) fn alias_lookup(value: Option<&str>) -> Option<&str> {
    let value = value?;
    value
        .strip_prefix(DYNAMIC_DISPATCH_RESOLUTION_PREFIX)
        .unwrap_or(value)
        .strip_prefix(NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX)
}

pub(super) fn alias_hint<Cancel>(
    index: &ResolutionIndex,
    (context, reference): (&FileResolutionContext<'_>, &ExtractedReference),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(name) = alias_lookup(reference.resolution_name.as_deref()) else {
        return Ok(None);
    };
    if reference.kind != ReferenceKind::Calls || !javascript_family_name(&context.identity.language)
    {
        return Ok(None);
    }
    if cancelled() {
        return Err(StageItemFailure);
    }
    let target = index
        .native_bridges
        .aliases
        .get(name)
        .and_then(Option::as_ref);
    Ok(target.map(|id| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: id.clone(),
            kind: SymbolKind::Method,
            confidence: CONVENTION_CONFIDENCE,
            provenance: ALIAS_PROVENANCE,
        })
    }))
}

#[derive(Default)]
pub(super) struct BridgeIndex {
    physical: HashMap<SymbolId, PhysicalEndpoint>,
    aliases: HashMap<String, Option<SymbolId>>,
}

struct PhysicalEndpoint {
    declaration: ResolutionCandidate,
    native_module: Option<String>,
}

pub(super) fn prepare<Cancel>(
    input: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let aliases = aliases(input.index, input.budget, cancelled)?;
    input.index.native_bridges.aliases = aliases;
    let wanted = needed_parents(input.index, input.budget, cancelled)?;
    if wanted.is_empty() {
        return Ok(());
    }
    let declarations = declarations(input.index, (&wanted, input.budget), cancelled)?;
    let physical = physical_methods(input.index, (&declarations, input.budget), cancelled)?;
    input.index.native_bridges.physical = physical;
    Ok(())
}

fn aliases<Cancel>(
    index: &ResolutionIndex,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<HashMap<String, Option<SymbolId>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut aliases = HashMap::<String, Option<SymbolId>>::new();
    for (name, candidates) in ordered_resolution_candidates(index) {
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if let Some(module) = alias_module(index, candidate) {
                let name = format!("{module}::{name}");
                record_alias(&mut aliases, budget, (name, candidate))?;
            }
        }
    }
    Ok(aliases)
}

fn record_alias(
    aliases: &mut HashMap<String, Option<SymbolId>>,
    budget: &mut ResolveBudget,
    (name, candidate): (String, &ResolutionCandidate),
) -> Result<(), StageItemFailure> {
    if let Some(previous) = aliases.get_mut(&name) {
        if previous.as_ref() != Some(&candidate.symbol_id) {
            *previous = None;
        }
    } else {
        budget.charge(MAP_ENTRY_BYTES + usize_to_u64(name.len()))?;
        aliases.try_reserve(1).map_err(|_| StageItemFailure)?;
        aliases.insert(name, Some(candidate.symbol_id.clone()));
    }
    Ok(())
}

fn alias_module<'candidate>(
    index: &ResolutionIndex,
    candidate: &'candidate ResolutionCandidate,
) -> Option<&'candidate str> {
    if candidate.kind != SymbolKind::Method
        || !index
            .modules
            .files
            .get(&candidate.file_id)
            .is_some_and(|file| native_bridge_target_language(&file.language))
    {
        return None;
    }
    ["::react-native-method::", "::expo-module-method::"]
        .iter()
        .find_map(|tag| tagged_framework_module(&candidate.qualified_name, tag))
}

fn declarations<'index, Cancel>(
    index: &'index ResolutionIndex,
    (wanted, budget): (&BTreeSet<&SymbolId>, &mut ResolveBudget),
    cancelled: &mut Cancel,
) -> Result<HashMap<SymbolId, &'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut declarations = HashMap::new();
    for (name, candidates) in ordered_resolution_candidates(index) {
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if wanted.contains(&candidate.symbol_id)
                && matches!(candidate.kind, SymbolKind::Method | SymbolKind::Function)
                && !framework_landmark_candidate(candidate)
            {
                budget.charge(MAP_ENTRY_BYTES)?;
                declarations.try_reserve(1).map_err(|_| StageItemFailure)?;
                declarations.insert(candidate.symbol_id.clone(), candidate);
            }
        }
    }
    Ok(declarations)
}

fn physical_methods<Cancel>(
    index: &ResolutionIndex,
    (declarations, budget): (&HashMap<SymbolId, &ResolutionCandidate>, &mut ResolveBudget),
    cancelled: &mut Cancel,
) -> Result<HashMap<SymbolId, PhysicalEndpoint>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut physical = HashMap::new();
    for (name, candidates) in ordered_resolution_candidates(index) {
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let Some(parent) = physical_parent(index, (declarations, candidate)) else {
                continue;
            };
            let module =
                tagged_framework_module(&candidate.qualified_name, "::react-native-method::");
            budget.charge(
                MAP_ENTRY_BYTES
                    + usize_to_u64(
                        parent.qualified_name.len()
                            + parent.signature.len()
                            + module.map_or(0, str::len),
                    ),
            )?;
            physical.try_reserve(1).map_err(|_| StageItemFailure)?;
            physical.insert(
                candidate.symbol_id.clone(),
                PhysicalEndpoint {
                    declaration: parent.clone(),
                    native_module: module.map(str::to_owned),
                },
            );
        }
    }
    Ok(physical)
}

fn physical_parent<'index>(
    index: &ResolutionIndex,
    (declarations, candidate): (
        &HashMap<SymbolId, &'index ResolutionCandidate>,
        &ResolutionCandidate,
    ),
) -> Option<&'index ResolutionCandidate> {
    if !physical_landmark(candidate) {
        return None;
    }
    index
        .parents
        .get(&candidate.symbol_id)
        .and_then(|id| declarations.get(id))
        .copied()
        .filter(|parent| {
            parent.file_id == candidate.file_id
                && parent.declaration_span.0 <= candidate.declaration_span.0
                && candidate.declaration_span.1 <= parent.declaration_span.1
        })
}

fn needed_parents<'index, Cancel>(
    index: &'index ResolutionIndex,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<BTreeSet<&'index SymbolId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut wanted = BTreeSet::new();
    for (name, candidates) in ordered_resolution_candidates(index) {
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if physical_landmark(candidate)
                && let Some(parent) = index.parents.get(&candidate.symbol_id)
            {
                budget.charge(MAP_ENTRY_BYTES)?;
                wanted.insert(parent);
            }
        }
    }
    Ok(wanted)
}

fn physical_landmark(candidate: &ResolutionCandidate) -> bool {
    [
        "::react-native-method::",
        "::objc-swift-method::",
        "::swift-objc-method::",
    ]
    .iter()
    .any(|tag| candidate.qualified_name.contains(tag))
}

pub(super) fn physical_candidate<'index>(
    index: &'index ResolutionIndex,
    candidate: &'index ResolutionCandidate,
) -> &'index ResolutionCandidate {
    index
        .native_bridges
        .physical
        .get(&candidate.symbol_id)
        .map_or(candidate, |endpoint| &endpoint.declaration)
}

pub(super) fn physical_resolution(
    index: &ResolutionIndex,
    (context, reference, mut resolution): (
        &FileResolutionContext<'_>,
        &ExtractedReference,
        ReferenceResolution,
    ),
) -> ReferenceResolution {
    if let Some(target) = resolution.target.as_mut()
        && let Some(endpoint) = index.native_bridges.physical.get(&target.symbol_id)
        && proven_endpoint(index, (context, reference), (&target.symbol_id, endpoint))
        && index
            .modules
            .files
            .get(&endpoint.declaration.file_id)
            .is_some_and(|file| matches!(file.language.as_str(), "java" | "kotlin"))
    {
        let physical = &endpoint.declaration;
        target.symbol_id = physical.symbol_id.clone();
        target.kind = physical.kind;
        target.confidence = target.confidence.min(CONVENTION_CONFIDENCE);
        target.provenance = PHYSICAL_METHOD_PROVENANCE;
    }
    resolution
}

fn proven_endpoint(
    index: &ResolutionIndex,
    (context, reference): (&FileResolutionContext<'_>, &ExtractedReference),
    (target, endpoint): (&SymbolId, &PhysicalEndpoint),
) -> bool {
    let lookup = alias_lookup(reference.resolution_name.as_deref()).or_else(|| {
        reference.resolution_name.as_deref().map(|name| {
            name.strip_prefix(DYNAMIC_DISPATCH_RESOLUTION_PREFIX)
                .unwrap_or(name)
        })
    });
    let proven_alias = lookup
        .and_then(|name| index.native_bridges.aliases.get(name))
        .and_then(Option::as_ref);
    if proven_alias == Some(target) {
        return true;
    }
    javascript_member_resolution::binding_name(index, (&context.identity.file_id, reference))
        .and_then(|name| name.strip_prefix("NativeModules."))
        .and_then(|name| name.split_once('.'))
        .is_some_and(|(module, member)| {
            endpoint.native_module.as_deref() == Some(module) && member == reference.name
        })
}

pub(super) fn append<Cancel>(
    input: &mut ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for (name, candidates) in ordered_resolution_candidates(input.index) {
        if name.contains("::") {
            continue;
        }
        for source in candidates {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            if let Some(target) = input.index.native_bridges.physical.get(&source.symbol_id) {
                append_framework_edge(
                    input.facts,
                    input.budget,
                    FrameworkEdgeInput {
                        source,
                        target: &target.declaration,
                        confidence: CONVENTION_CONFIDENCE,
                        provenance: PHYSICAL_METHOD_PROVENANCE,
                    },
                )?;
            }
        }
    }
    Ok(())
}
